use std::collections::BTreeMap;
use std::io::Read;

use jxl_bitstream::{read_bits, Bitstream, Bundle, header::Headers};

mod error;
pub mod filter;
pub mod data;
pub mod header;

pub use error::{Error, Result};
pub use header::FrameHeader;
pub use data::Toc;

use crate::data::*;

#[derive(Debug)]
pub struct Frame<'a> {
    image_header: &'a Headers,
    header: FrameHeader,
    toc: Toc,
    data: FrameData,
    pass_shifts: BTreeMap<u32, (i32, i32)>,
    pending_groups: BTreeMap<TocGroupKind, Vec<u8>>,
}

impl<'a> Bundle<&'a Headers> for Frame<'a> {
    type Error = crate::Error;

    fn parse<R: Read>(bitstream: &mut Bitstream<R>, image_header: &'a Headers) -> Result<Self> {
        bitstream.zero_pad_to_byte()?;
        let header = read_bits!(bitstream, Bundle(FrameHeader), image_header)?;
        let toc = read_bits!(bitstream, Bundle(Toc), &header)?;
        let data = FrameData::new(&header);

        let passes = &header.passes;
        let mut pass_shifts = BTreeMap::new();
        let mut maxshift = 3i32;
        for (&downsample, &last_pass) in passes.downsample.iter().zip(&passes.last_pass) {
            let minshift = downsample.trailing_zeros() as i32;
            pass_shifts.insert(last_pass, (minshift, maxshift));
            maxshift = minshift;
        }
        pass_shifts.insert(header.passes.num_passes - 1, (0i32, maxshift));

        Ok(Self {
            image_header,
            header,
            toc,
            data,
            pass_shifts,
            pending_groups: Default::default(),
        })
    }
}

impl Frame<'_> {
    pub fn header(&self) -> &FrameHeader {
        &self.header
    }

    pub fn toc(&self) -> &Toc {
        &self.toc
    }

    pub fn data(&self) -> &FrameData {
        &self.data
    }
}

impl Frame<'_> {
    pub fn load_cropped<R: Read>(
        &mut self,
        bitstream: &mut Bitstream<R>,
        region: Option<(u32, u32, u32, u32)>,
    ) -> Result<()> {
        if self.toc.is_single_entry() {
            let group = self.toc.lf_global();
            self.read_group(bitstream, group)?;
            return Ok(());
        }

        let mut region = if region.is_some() && self.header.have_crop {
            region.map(|(left, top, width, height)| (
                left.saturating_add_signed(-self.header.x0),
                top.saturating_add_signed(-self.header.y0),
                width,
                height,
            ))
        } else {
            region
        };

        let mut it = self.toc.iter_bitstream_order();
        let mut pending = Vec::new();
        for group in &mut it {
            bitstream.skip_to_bookmark(group.offset)?;
            if matches!(group.kind, TocGroupKind::LfGlobal) {
                self.read_group(bitstream, group)?;
                break;
            }

            let mut buf = vec![0u8; group.size as usize];
            bitstream.read_bytes_aligned(&mut buf)?;
            pending.push((group, Some(buf)));
        }

        let lf_global = self.data.lf_global.as_ref().unwrap();
        if lf_global.gmodular.modular.has_delta_palette() {
            if region.take().is_some() {
                eprintln!("GlobalModular has delta palette, forcing full decode");
            }
        } else if lf_global.gmodular.modular.has_squeeze() {
            if let Some((left, top, width, height)) = &mut region {
                *width += *left;
                *height += *top;
                *left = 0;
                *top = 0;
                eprintln!("GlobalModular has squeeze, decoding from top-left");
            }
        }
        if let Some(region) = &region {
            eprintln!("Cropped decoding: {:?}", region);
        }

        for (group, buf) in pending.into_iter().chain(it.map(|v| (v, None))) {
            if let Some(region) = region {
                match group.kind {
                    TocGroupKind::LfGroup(lf_group_idx) => {
                        let lf_group_dim = self.header.lf_group_dim();
                        let lf_group_per_row = self.header.lf_groups_per_row();
                        let group_left = (lf_group_idx % lf_group_per_row) * lf_group_dim;
                        let group_top = (lf_group_idx / lf_group_per_row) * lf_group_dim;
                        if !is_aabb_collides(region, (group_left, group_top, lf_group_dim, lf_group_dim)) {
                            continue;
                        }
                    },
                    TocGroupKind::GroupPass { group_idx, .. } => {
                        let group_dim = self.header.group_dim();
                        let group_per_row = self.header.groups_per_row();
                        let group_left = (group_idx % group_per_row) * group_dim;
                        let group_top = (group_idx / group_per_row) * group_dim;
                        if !is_aabb_collides(region, (group_left, group_top, group_dim, group_dim)) {
                            continue;
                        }
                    },
                    _ => {},
                }
            }

            if let Some(buf) = buf {
                let mut bitstream = Bitstream::new(std::io::Cursor::new(buf));
                self.read_group(&mut bitstream, group)?;
            } else {
                bitstream.skip_to_bookmark(group.offset)?;
                self.read_group(bitstream, group)?;
            }
        }

        Ok(())
    }

    pub fn load_all<R: Read>(&mut self, bitstream: &mut Bitstream<R>) -> Result<()> {
        if self.toc.is_single_entry() {
            let group = self.toc.lf_global();
            bitstream.skip_to_bookmark(group.offset)?;
            self.read_group(bitstream, group)?;
            return Ok(());
        }

        for group in self.toc.iter_bitstream_order() {
            bitstream.skip_to_bookmark(group.offset)?;
            self.read_group(bitstream, group)?;
        }

        Ok(())
    }

    #[cfg(feature = "mt")]
    pub fn load_cropped_par<R: Read + Send>(
        &mut self,
        bitstream: &mut Bitstream<R>,
        region: Option<(u32, u32, u32, u32)>,
    ) -> Result<()> {
        use rayon::prelude::*;

        if self.toc.is_single_entry() {
            let group = self.toc.lf_global();
            bitstream.skip_to_bookmark(group.offset)?;
            self.read_group(bitstream, group)?;
            return Ok(());
        }

        let mut region = if region.is_some() && self.header.have_crop {
            region.map(|(left, top, width, height)| (
                left.saturating_add_signed(-self.header.x0),
                top.saturating_add_signed(-self.header.y0),
                width,
                height,
            ))
        } else {
            region
        };

        let mut lf_global = self.data.lf_global.take();
        let mut hf_global = self.data.hf_global.take();

        let (lf_group_tx, lf_group_rx) = crossbeam_channel::unbounded();
        let (pass_group_tx, pass_group_rx) = crossbeam_channel::unbounded();

        let mut it = self.toc.iter_bitstream_order();
        while lf_global.is_none() || hf_global.is_none() {
            let group = it.next().expect("lf_global or hf_global not found?");
            bitstream.skip_to_bookmark(group.offset)?;

            match group.kind {
                TocGroupKind::LfGlobal => {
                    let mut buf = vec![0u8; group.size as usize];
                    bitstream.read_bytes_aligned(&mut buf)?;
                    let mut bitstream = Bitstream::new(std::io::Cursor::new(buf));
                    lf_global = Some(self.read_lf_global(&mut bitstream)?);
                },
                TocGroupKind::HfGlobal => {
                    hf_global = Some(self.read_hf_global(bitstream)?);
                },
                TocGroupKind::LfGroup(lf_group_idx) => {
                    let mut buf = vec![0u8; group.size as usize];
                    bitstream.read_bytes_aligned(&mut buf)?;
                    lf_group_tx.send((lf_group_idx, buf)).unwrap();
                },
                TocGroupKind::GroupPass { pass_idx, group_idx } => {
                    let mut buf = vec![0u8; group.size as usize];
                    bitstream.read_bytes_aligned(&mut buf)?;
                    pass_group_tx.send((pass_idx, group_idx, buf)).unwrap();
                },
                _ => unreachable!(),
            }
        }

        self.data.lf_global = lf_global;
        self.data.hf_global = hf_global;
        let lf_global = self.data.lf_global.as_ref().unwrap();
        let hf_global = self.data.hf_global.as_ref().unwrap().as_ref();

        if lf_global.gmodular.modular.has_delta_palette() {
            if region.take().is_some() {
                eprintln!("GlobalModular has delta palette, forcing full decode");
            }
        } else if lf_global.gmodular.modular.has_squeeze() {
            if let Some((left, top, width, height)) = &mut region {
                *width += *left;
                *height += *top;
                *left = 0;
                *top = 0;
                eprintln!("GlobalModular has squeeze, decoding from top-left");
            }
        }
        if let Some(region) = &region {
            eprintln!("Cropped decoding: {:?}", region);
        }

        let mut lf_groups = Ok(BTreeMap::new());
        let mut pass_groups = Ok(BTreeMap::new());
        let io_result = rayon::scope(|scope| -> Result<()> {
            let lf_group_tx = lf_group_tx;
            let pass_group_tx = pass_group_tx;

            scope.spawn(|_| {
                lf_groups = lf_group_rx
                    .into_iter()
                    .par_bridge()
                    .filter(|(lf_group_idx, _)| {
                        let Some(region) = region else { return true; };
                        let lf_group_dim = self.header.lf_group_dim();
                        let lf_group_per_row = self.header.lf_groups_per_row();
                        let group_left = (lf_group_idx % lf_group_per_row) * lf_group_dim;
                        let group_top = (lf_group_idx / lf_group_per_row) * lf_group_dim;
                        is_aabb_collides(region, (group_left, group_top, lf_group_dim, lf_group_dim))
                    })
                    .map(|(lf_group_idx, buf)| {
                        let mut bitstream = Bitstream::new(std::io::Cursor::new(buf));
                        let lf_group = self.read_lf_group(&mut bitstream, lf_global, lf_group_idx)?;
                        Ok((lf_group_idx, lf_group))
                    })
                    .collect::<Result<BTreeMap<_, _>>>();
            });
            scope.spawn(|_| {
                pass_groups = pass_group_rx
                    .into_iter()
                    .par_bridge()
                    .filter(|(_, group_idx, _)| {
                        let Some(region) = region else { return true; };
                        let group_dim = self.header.group_dim();
                        let group_per_row = self.header.groups_per_row();
                        let group_left = (group_idx % group_per_row) * group_dim;
                        let group_top = (group_idx / group_per_row) * group_dim;
                        is_aabb_collides(region, (group_left, group_top, group_dim, group_dim))
                    })
                    .map(|(pass_idx, group_idx, buf)| {
                        let mut bitstream = Bitstream::new(std::io::Cursor::new(buf));
                        let pass_group = self.read_group_pass(&mut bitstream, lf_global, hf_global, pass_idx, group_idx)?;
                        Ok(((pass_idx, group_idx), pass_group))
                    })
                    .collect::<Result<BTreeMap<_, _>>>();
            });

            for group in it {
                bitstream.skip_to_bookmark(group.offset)?;
                let mut buf = vec![0u8; group.size as usize];
                bitstream.read_bytes_aligned(&mut buf)?;

                match group.kind {
                    TocGroupKind::LfGroup(lf_group_idx) =>
                        lf_group_tx.send((lf_group_idx, buf)).unwrap(),
                    TocGroupKind::GroupPass { pass_idx, group_idx } =>
                        pass_group_tx.send((pass_idx, group_idx, buf)).unwrap(),
                    _ => { /* ignore */ },
                }
            }

            Ok(())
        });

        io_result?;
        self.data.lf_group = lf_groups?;
        self.data.group_pass = pass_groups?;

        Ok(())
    }

    #[cfg(feature = "mt")]
    pub fn load_all_par<R: Read + Send>(&mut self, bitstream: &mut Bitstream<R>) -> Result<()> {
        self.load_cropped_par(bitstream, None)
    }

    pub fn read_lf_global<R: Read>(&mut self, bitstream: &mut Bitstream<R>) -> Result<LfGlobal> {
        read_bits!(bitstream, Bundle(LfGlobal), (self.image_header, &self.header))
    }

    pub fn read_lf_group<R: Read>(&self, bitstream: &mut Bitstream<R>, lf_global: &LfGlobal, lf_group_idx: u32) -> Result<LfGroup> {
        let lf_group_params = LfGroupParams::new(&self.header, lf_global, lf_group_idx);
        read_bits!(bitstream, Bundle(LfGroup), lf_group_params)
    }

    pub fn read_hf_global<R: Read>(&self, bitstream: &mut Bitstream<R>) -> Result<Option<HfGlobal>> {
        let has_hf_global = self.header.encoding == crate::header::Encoding::VarDct;
        let hf_global = if has_hf_global {
            todo!()
        } else {
            None
        };
        Ok(hf_global)
    }

    pub fn read_group_pass<R: Read>(&self, bitstream: &mut Bitstream<R>, lf_global: &LfGlobal, hf_global: Option<&HfGlobal>, pass_idx: u32, group_idx: u32) -> Result<PassGroup> {
        let shift = self.pass_shifts.get(&pass_idx).copied();
        let params = PassGroupParams::new(
            &self.header,
            lf_global,
            hf_global,
            pass_idx,
            group_idx,
            shift,
        );
        read_bits!(bitstream, Bundle(PassGroup), params)
    }

    pub fn read_group<R: Read>(&mut self, bitstream: &mut Bitstream<R>, group: TocGroup) -> Result<()> {
        let has_hf_global = self.header.encoding == crate::header::Encoding::VarDct;
        match group.kind {
            TocGroupKind::All => {
                let lf_global = self.read_lf_global(bitstream)?;
                let lf_group = self.read_lf_group(bitstream, &lf_global, 0)?;
                let hf_global = self.read_hf_global(bitstream)?;
                let group_pass = self.read_group_pass(bitstream, &lf_global, hf_global.as_ref(), 0, 0)?;

                self.data.lf_global = Some(lf_global);
                self.data.lf_group.insert(0, lf_group);
                self.data.hf_global = Some(hf_global);
                self.data.group_pass.insert((0, 0), group_pass);

                Ok(())
            },
            TocGroupKind::LfGlobal => {
                let lf_global = read_bits!(bitstream, Bundle(LfGlobal), (self.image_header, &self.header))?;
                self.data.lf_global = Some(lf_global);
                self.try_pending_blocks()?;
                Ok(())
            },
            TocGroupKind::LfGroup(lf_group_idx) => {
                let Some(lf_global) = &self.data.lf_global else {
                    let mut buf = vec![0u8; group.size as usize];
                    bitstream.read_bytes_aligned(&mut buf)?;
                    self.pending_groups.insert(group.kind, buf);
                    return Ok(());
                };
                let lf_group = self.read_lf_group(bitstream, lf_global, lf_group_idx)?;
                self.data.lf_group.insert(lf_group_idx, lf_group);
                Ok(())
            },
            TocGroupKind::HfGlobal => {
                let hf_global = self.read_hf_global(bitstream)?;
                self.data.hf_global = Some(hf_global);
                Ok(())
            },
            TocGroupKind::GroupPass { pass_idx, group_idx } => {
                let (Some(lf_global), Some(hf_global)) = (&self.data.lf_global, &self.data.hf_global) else {
                    let mut buf = vec![0u8; group.size as usize];
                    bitstream.read_bytes_aligned(&mut buf)?;
                    self.pending_groups.insert(group.kind, buf);
                    return Ok(());
                };

                let group_pass = self.read_group_pass(bitstream, lf_global, hf_global.as_ref(), pass_idx, group_idx)?;
                self.data.group_pass.insert((pass_idx, group_idx), group_pass);
                Ok(())
            },
        }
    }

    fn try_pending_blocks(&mut self) -> Result<()> {
        // TODO: parse pending blocks
        Ok(())
    }

    pub fn complete(&mut self) -> Result<()> {
        self.data.complete(&self.header)?;
        Ok(())
    }

    pub fn rgba_be_interleaved<F>(&self, f: F) -> Result<()>
    where
        F: FnMut(&[u8]) -> Result<()>,
    {
        let bit_depth = self.image_header.metadata.bit_depth.bits_per_sample();
        let modular_channels = self.data.lf_global.as_ref().unwrap().gmodular.modular.image().channel_data();
        let alpha = self.image_header.metadata.alpha();

        let (rgb, a) = if self.header.encoding == crate::header::Encoding::VarDct {
            todo!()
        } else {
            let rgb = [&modular_channels[0], &modular_channels[1], &modular_channels[2]];
            let a = alpha.map(|idx| &modular_channels[3 + idx]);
            (rgb, a)
        };

        jxl_grid::rgba_be_interleaved(rgb, a, bit_depth, f)
    }
}

#[derive(Debug)]
pub struct FrameData {
    pub lf_global: Option<LfGlobal>,
    pub lf_group: BTreeMap<u32, LfGroup>,
    pub hf_global: Option<Option<HfGlobal>>,
    pub group_pass: BTreeMap<(u32, u32), PassGroup>,
}

impl FrameData {
    fn new(frame_header: &FrameHeader) -> Self {
        let has_hf_global = frame_header.encoding == crate::header::Encoding::VarDct;
        let hf_global = if has_hf_global {
            None
        } else {
            Some(None)
        };

        Self {
            lf_global: None,
            lf_group: Default::default(),
            hf_global,
            group_pass: Default::default(),
        }
    }

    fn complete(&mut self, frame_header: &FrameHeader) -> Result<&mut Self> {
        let Self {
            lf_global,
            lf_group,
            hf_global,
            group_pass,
        } = self;

        let Some(lf_global) = lf_global else {
            return Err(Error::IncompleteFrameData { field: "lf_global" });
        };
        for lf_group in std::mem::take(lf_group).into_values() {
            lf_global.gmodular.modular.copy_from_modular(lf_group.mlf_group);
        }
        for group in std::mem::take(group_pass).into_values() {
            lf_global.gmodular.modular.copy_from_modular(group.modular);
        }

        lf_global.apply_modular_inverse_transform();

        // TODO: perform vardct

        Ok(self)
    }
}

fn is_aabb_collides(rect0: (u32, u32, u32, u32), rect1: (u32, u32, u32, u32)) -> bool {
    let (x0, y0, w0, h0) = rect0;
    let (x1, y1, w1, h1) = rect1;
    (x0 < x1 + w1) && (x0 + w0 > x1) && (y0 < y1 + h1) && (y0 + h0 > y1)
}
