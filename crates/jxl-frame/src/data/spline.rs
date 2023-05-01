#![allow(clippy::needless_range_loop)]

use std::{
    f32::consts::SQRT_2,
    fmt::Display,
    io::Read,
    ops::{Add, Mul, Sub},
};

use jxl_bitstream::{unpack_signed, Bitstream, Bundle};
use jxl_coding::Decoder;

use crate::{FrameHeader, Result};

const MAX_NUM_SPLINES: usize = 1 << 24;
const MAX_NUM_CONTROL_POINTS: usize = 1 << 20;

/// Holds quantized splines
#[derive(Debug)]
pub struct Splines {
    pub quant_splines: Vec<QuantSpline>,
    pub quant_adjust: i32,
}

/// 2D Point in f32 coordinates
#[derive(Debug, Default, Clone, Copy)]
pub struct Point {
    pub x: f32,
    pub y: f32,
}

/// Holds control point coordinates and dequantized DCT32 coefficients of XYB channels, σ parameter of the spline
#[derive(Debug)]
pub struct Spline {
    pub points: Vec<Point>,
    pub xyb_dct: [[f32; 32]; 3],
    pub sigma_dct: [f32; 32],
}

pub struct SplineArc {
    pub point: Point,
    pub length: f32,
}

/// Holds delta-endcoded control points coordinates (without starting point) and quantized DCT32 coefficients
///
/// Use [`QuantSpline::dequant`] to get normal [Spline]
#[derive(Debug, Default, Clone)]
pub struct QuantSpline {
    start_point: (i32, i32),
    points_deltas: Vec<(i32, i32)>,
    xyb_dct: [[i32; 32]; 3],
    sigma_dct: [i32; 32],
}

impl Bundle<&FrameHeader> for Splines {
    type Error = crate::Error;

    fn parse<R: Read>(bitstream: &mut Bitstream<R>, header: &FrameHeader) -> Result<Self> {
        let mut decoder = jxl_coding::Decoder::parse(bitstream, 6)?;
        decoder.begin(bitstream)?;
        let num_pixels = (header.width * header.height) as usize;

        let num_splines = (decoder.read_varint(bitstream, 2)? + 1) as usize;

        let max_num_splines = usize::min(MAX_NUM_SPLINES, num_pixels / 4);
        if num_splines > max_num_splines {
            return Err(crate::Error::TooManySplines(num_splines));
        }

        let mut start_points = vec![(0i32, 0i32); num_splines];
        for i in 0..num_splines {
            let mut x = decoder.read_varint(bitstream, 1)? as i32;
            let mut y = decoder.read_varint(bitstream, 1)? as i32;
            if i != 0 {
                x = unpack_signed(x as u32) + start_points[i - 1].0;
                y = unpack_signed(y as u32) + start_points[i - 1].1;
            }
            start_points[i].0 = x;
            start_points[i].1 = y;
        }

        let quant_adjust = unpack_signed(decoder.read_varint(bitstream, 0)?);

        let mut splines: Vec<QuantSpline> = Vec::with_capacity(num_splines);
        for start_point in start_points {
            let mut spline = QuantSpline::new(start_point);
            spline.decode(&mut decoder, bitstream, num_pixels)?;
            splines.push(spline);
        }

        Ok(Self {
            quant_adjust,
            quant_splines: splines,
        })
    }
}

impl QuantSpline {
    fn new(start_point: (i32, i32)) -> Self {
        Self {
            start_point,
            points_deltas: Vec::new(),
            xyb_dct: [[0; 32]; 3],
            sigma_dct: [0; 32],
        }
    }

    fn decode<R: Read>(
        &mut self,
        decoder: &mut Decoder,
        bitstream: &mut Bitstream<R>,
        num_pixels: usize,
    ) -> Result<()> {
        let num_points = decoder.read_varint(bitstream, 3)? as usize;

        let max_num_points = usize::min(MAX_NUM_CONTROL_POINTS, num_pixels / 2);
        if num_points > max_num_points {
            return Err(crate::Error::TooManySplinePoints(num_points));
        }

        self.points_deltas.resize(num_points, (0, 0));

        for delta in &mut self.points_deltas {
            delta.0 = unpack_signed(decoder.read_varint(bitstream, 4)?);
            delta.1 = unpack_signed(decoder.read_varint(bitstream, 4)?);
        }
        for color_dct in &mut self.xyb_dct {
            for i in color_dct {
                *i = unpack_signed(decoder.read_varint(bitstream, 5)?);
            }
        }
        for i in &mut self.sigma_dct {
            *i = unpack_signed(decoder.read_varint(bitstream, 5)?);
        }
        Ok(())
    }

    pub fn dequant(
        &self,
        quant_adjust: i32,
        base_correlations_xb: Option<(f32, f32)>,
        estimated_area: &mut u64,
    ) -> Spline {
        let mut manhattan_distance = 0u64;
        let mut points = Vec::with_capacity(self.points_deltas.len() + 1);

        let mut cur_value = self.start_point;
        points.push(Point::new(cur_value.0 as f32, cur_value.1 as f32));
        let mut cur_delta = (0, 0);
        for delta in &self.points_deltas {
            cur_delta.0 += delta.0;
            cur_delta.1 += delta.1;
            manhattan_distance += (cur_delta.0.abs() + cur_delta.1.abs()) as u64;
            cur_value.0 += cur_delta.0;
            cur_value.1 += cur_delta.1;
            points.push(Point::new(cur_value.0 as f32, cur_value.1 as f32));
        }

        let mut xyb_dct = [[0f32; 32]; 3];
        let mut sigma_dct = [0f32; 32];
        let mut width_estimate = 0u64;

        let quant_adjust = quant_adjust as f32;
        let inverted_qa = if quant_adjust >= 0.0 {
            1.0 / (1.0 + quant_adjust / 8.0)
        } else {
            1.0 - quant_adjust / 8.0
        };

        const CHANNEL_WEIGHTS: [f32; 4] = [0.0042, 0.075, 0.07, 0.3333];
        for chan_idx in 0..3 {
            for i in 0..32 {
                xyb_dct[chan_idx][i] =
                    self.xyb_dct[chan_idx][i] as f32 * CHANNEL_WEIGHTS[chan_idx] * inverted_qa;
            }
        }
        let (corr_x, corr_b) = base_correlations_xb.unwrap_or((0.0, 1.0));
        for i in 0..32 {
            xyb_dct[0][i] += corr_x * xyb_dct[1][i];
            xyb_dct[2][i] += corr_b * xyb_dct[1][i];
        }

        // This block is only needed to check conformance with the levels
        let log_color = {
            let mut color_xyb = [0u64; 3];
            for chan_idx in 0..3 {
                for i in 0..32 {
                    color_xyb[chan_idx] +=
                        (self.xyb_dct[chan_idx][i].abs() as f32 * inverted_qa).ceil() as u64;
                }
            }
            color_xyb[0] += corr_x.abs().ceil() as u64 * color_xyb[1];
            color_xyb[2] += corr_b.abs().ceil() as u64 * color_xyb[1];
            u64::max(
                1u64,
                log2_ceil(1u64 + color_xyb.into_iter().max().unwrap()) as u64,
            )
        };

        for i in 0..32 {
            sigma_dct[i] = self.sigma_dct[i] as f32 * CHANNEL_WEIGHTS[3] * inverted_qa;
            let weight = u64::max(
                1u64,
                (self.sigma_dct[i].abs() as f32 * inverted_qa).ceil() as u64,
            );
            width_estimate += weight * weight * log_color;
        }

        *estimated_area += width_estimate * manhattan_distance;

        Spline {
            points,
            xyb_dct,
            sigma_dct,
        }
    }
}

impl Spline {
    pub fn get_samples(&self) -> Vec<SplineArc> {
        let upsampled_points = self.get_upsampled_points();

        let mut current = upsampled_points[0];
        let mut next_idx = 0;
        let mut all_samples = vec![SplineArc {
            point: current,
            length: 1f32,
        }];

        while next_idx < upsampled_points.len() {
            let mut prev = current;
            let mut arclength = 0f32;
            loop {
                if next_idx >= upsampled_points.len() {
                    all_samples.push(SplineArc {
                        point: prev,
                        length: arclength,
                    });
                    break;
                }
                let next = upsampled_points[next_idx];
                let arclength_to_next = (next - prev).norm();
                if arclength + arclength_to_next >= 1.0 {
                    current = prev
                        + ((upsampled_points[next_idx] - prev)
                            * ((1.0 - arclength) / arclength_to_next));
                    all_samples.push(SplineArc {
                        point: current,
                        length: 1.0,
                    });
                    break;
                }
                arclength += arclength_to_next;
                prev = next;
                next_idx += 1;
            }
        }
        all_samples
    }

    fn get_upsampled_points(&self) -> Vec<Point> {
        let s = &self.points;
        if s.len() == 1 {
            return vec![s[0]];
        }

        let mut extended = Vec::with_capacity(s.len() + 2);

        extended.push(s[1].mirror(&s[0]));
        extended.append(&mut s.clone());
        extended.push(s[s.len() - 2].mirror(&s[s.len() - 1]));

        let mut upsampled = Vec::with_capacity(16 * (extended.len() - 3) + 1);

        for i in 0..extended.len() - 3 {
            let mut p: [Point; 4] = Default::default();
            let mut t: [f32; 4] = Default::default();
            let mut a: [Point; 4] = Default::default();
            let mut b: [Point; 3] = Default::default();

            p.clone_from_slice(&extended[i..i + 4]);
            upsampled.push(p[1]);
            t[0] = 0f32;

            for k in 1..4 {
                t[k] = t[k - 1] + (p[k] - p[k - 1]).norm_squared().powf(0.25);
            }

            for step in 1..16 {
                let knot = t[1] + (step as f32 / 16.0) * (t[2] - t[1]);
                for k in 0..3 {
                    a[k] = p[k] + ((p[k + 1] - p[k]) * ((knot - t[k]) / (t[k + 1] - t[k])));
                }
                for k in 0..2 {
                    b[k] = a[k] + ((a[k + 1] - a[k]) * ((knot - t[k]) / (t[k + 2] - t[k])));
                }
                upsampled.push(b[0] + ((b[1] - b[0]) * ((knot - t[1]) / (t[2] - t[1]))));
            }
        }
        upsampled.push(s[s.len() - 1]);
        upsampled
    }
}

// Done in jxl_from_tree syntax
impl Display for Spline {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        writeln!(f, "Spline")?;
        for i in self.xyb_dct.iter().chain(&[self.sigma_dct]) {
            for val in i {
                write!(f, "{} ", val)?;
            }
            writeln!(f)?;
        }
        for point in &self.points {
            writeln!(f, "{} {}", point.x as i32, point.y as i32)?;
        }
        writeln!(f, "EndSpline")
    }
}

impl Point {
    pub fn new(x: f32, y: f32) -> Self {
        Self { x, y }
    }
    pub fn mirror(&self, center: &Self) -> Self {
        Self {
            x: center.x + center.x - self.x,
            y: center.y + center.y - self.y,
        }
    }

    pub fn norm_squared(&self) -> f32 {
        self.x * self.x + self.y * self.y
    }

    pub fn norm(&self) -> f32 {
        f32::sqrt(self.norm_squared())
    }
}

impl Add for Point {
    type Output = Self;
    fn add(self, rhs: Self) -> Self::Output {
        Self::new(self.x + rhs.x, self.y + rhs.y)
    }
}

impl Sub for Point {
    type Output = Self;
    fn sub(self, rhs: Self) -> Self::Output {
        Self::new(self.x - rhs.x, self.y - rhs.y)
    }
}

impl Mul<f32> for Point {
    type Output = Self;
    fn mul(self, rhs: f32) -> Self::Output {
        Self {
            x: self.x * rhs,
            y: self.y * rhs,
        }
    }
}

pub fn continuous_idct(dct: &[f32; 32], t: f32) -> f32 {
    let mut res = dct[0];
    for i in 1..32 {
        res += SQRT_2 * dct[i] * f32::cos((i as f32) * (std::f32::consts::PI / 32.0) * (t + 0.5));
    }
    res
}

/// Computes the error function
/// L1 error 7e-4.
#[allow(clippy::excessive_precision)]
pub fn erf(x: f32) -> f32 {
    let ax = x.abs();

    // Compute 1 - 1 / ((((x * a + b) * x + c) * x + d) * x + 1)**4
    let denom1 = ax * 7.77394369e-02 + 2.05260015e-04;
    let denom2 = denom1 * ax + 2.32120216e-01;
    let denom3 = denom2 * ax + 2.77820801e-01;
    let denom4 = denom3 * ax + 1.0;
    let denom5 = denom4 * denom4;
    let inv_denom5 = 1.0 / denom5;
    let result = -inv_denom5 * inv_denom5 + 1.0;

    // Change sign if needed.
    if x < 0.0 {
        -result
    } else {
        result
    }
}

fn log2_ceil(x: u64) -> u32 {
    x.next_power_of_two().trailing_zeros()
}
