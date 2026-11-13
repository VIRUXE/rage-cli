//! RDR2 PRD0 metadata uses the GTA V Meta schema/member records with a
//! shifted container header. Normalize only that header for the shared walker.
use super::{decompress, u32_at, u64_at};
use anyhow::{Context, Result, bail};
use rage_formats::{MetaDump, build_rsc7, dump_meta};

pub fn is_rsc8(data: &[u8]) -> bool {
    data.get(..4) == Some(&rage_formats::RSC8_MAGIC.to_le_bytes())
}

pub fn dump(data: &[u8]) -> Result<MetaDump> {
    if !is_rsc8(data) || data.len() < 16 {
        bail!("RDR2 metadata requires an RSC8 header");
    }
    let sys_flags = u32_at(data, 8)?;
    let gfx_flags = u32_at(data, 12)?;
    let sys_size = (sys_flags & 0xFFFFFFF0) as usize;
    let gfx_size = (gfx_flags & 0xFFFFFFF0) as usize;
    let size = sys_size
        .checked_add(gfx_size)
        .context("RDR2 resource size overflow")?;
    let flags = u32_at(data, 4)?;
    let compressor = (((flags >> 8) & 31) as u8 + 1) & 31;
    if flags & 0x00FF0000 != 0 {
        bail!(
            "loose RDR2 RSC8 resource is encrypted; extract it from the archive with --rdr2-keys first"
        );
    }
    let mut body = decompress(compressor, &data[16..], size)?;
    let sys = body
        .get_mut(..sys_size)
        .context("RDR2 system section truncated")?;
    if sys.len() < 0x60 || u32_at(sys, 0x10)? != 0x50524430 {
        bail!("RDR2 RSC8 resource does not contain PRD0 self-describing metadata");
    }
    if u32_at(sys, 0x14)? != b'y' as u32 {
        bail!("RDR2 metadata is not PC platform");
    }
    let root = u32::try_from(u64_at(sys, 0x20)?).context("RDR2 root block index overflow")?;
    let structs = u64_at(sys, 0x28)?;
    let enums = u64_at(sys, 0x30)?;
    let blocks = u64_at(sys, 0x38)?;
    if sys_size < 0x70 {
        bail!("RDR2 metadata system section too short");
    }
    sys[0x1C..0x20].copy_from_slice(&root.to_le_bytes());
    sys[0x20..0x28].copy_from_slice(&structs.to_le_bytes());
    sys[0x28..0x30].copy_from_slice(&enums.to_le_bytes());
    sys[0x30..0x38].copy_from_slice(&blocks.to_le_bytes());
    dump_meta(&build_rsc7(2, &body[..sys_size], &body[sys_size..]))
}
