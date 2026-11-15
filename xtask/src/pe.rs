//! Minimal PE/COFF import reader for the CRT and delay-load guards.
//!
//! Reads the DLL names of a Windows executable's **load-time** import directory (data directory 1)
//! and its **delay-load** import directory (data directory 13). This is the same information as
//! `dumpbin /dependents`, without needing the MSVC tools on `PATH` (CI runs it on a plain
//! `windows-latest` image). Only what the guards need is parsed; anything malformed is an error,
//! never a silent "no imports".

use std::path::Path;

/// DLL names imported by one image, in file order, as written in the image (case preserved).
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Imports {
    /// Load-time imports (mapped by the loader before `main`).
    pub load_time: Vec<String>,
    /// Delay-load imports (resolved by the delay-load helper on first call).
    pub delay_load: Vec<String>,
}

const IMAGE_DIRECTORY_ENTRY_IMPORT: usize = 1;
const IMAGE_DIRECTORY_ENTRY_DELAY_IMPORT: usize = 13;
const PE32_MAGIC: u16 = 0x10b;
const PE32_PLUS_MAGIC: u16 = 0x20b;
/// Upper bound on descriptors walked per directory (a real image has a few dozen).
const MAX_DESCRIPTORS: usize = 4096;

struct Section {
    va: u32,
    vsize: u32,
    raw_ptr: u32,
    raw_size: u32,
}

struct Image<'a> {
    bytes: &'a [u8],
    sections: Vec<Section>,
    image_base: u64,
    dirs: Vec<(u32, u32)>,
}

fn rd<const N: usize>(b: &[u8], off: usize) -> Result<[u8; N], String> {
    b.get(off..off.checked_add(N).ok_or("offset overflow")?)
        .and_then(|s| s.try_into().ok())
        .ok_or_else(|| format!("truncated image (read {N} bytes at {off:#x})"))
}
fn u16_at(b: &[u8], off: usize) -> Result<u16, String> {
    rd::<2>(b, off).map(u16::from_le_bytes)
}
fn u32_at(b: &[u8], off: usize) -> Result<u32, String> {
    rd::<4>(b, off).map(u32::from_le_bytes)
}
fn u64_at(b: &[u8], off: usize) -> Result<u64, String> {
    rd::<8>(b, off).map(u64::from_le_bytes)
}

impl<'a> Image<'a> {
    fn parse(bytes: &'a [u8]) -> Result<Self, String> {
        if bytes.get(0..2) != Some(b"MZ") {
            return Err("not a PE image (no MZ header)".into());
        }
        let pe = u32_at(bytes, 0x3c)? as usize;
        if bytes.get(pe..pe + 4) != Some(b"PE\0\0") {
            return Err("not a PE image (no PE signature)".into());
        }
        let coff = pe + 4;
        let nsections = usize::from(u16_at(bytes, coff + 2)?);
        let opt_size = usize::from(u16_at(bytes, coff + 16)?);
        let opt = coff + 20;
        let (image_base, ndirs_off, dirs_off) = match u16_at(bytes, opt)? {
            PE32_PLUS_MAGIC => (u64_at(bytes, opt + 24)?, opt + 108, opt + 112),
            PE32_MAGIC => (u64::from(u32_at(bytes, opt + 28)?), opt + 92, opt + 96),
            m => return Err(format!("unknown optional-header magic {m:#x}")),
        };
        let ndirs = (u32_at(bytes, ndirs_off)? as usize).min(16);
        let dirs = (0..ndirs)
            .map(|i| {
                Ok((
                    u32_at(bytes, dirs_off + i * 8)?,
                    u32_at(bytes, dirs_off + i * 8 + 4)?,
                ))
            })
            .collect::<Result<Vec<_>, String>>()?;
        let sec0 = opt + opt_size;
        let sections = (0..nsections)
            .map(|i| {
                let s = sec0 + i * 40;
                Ok(Section {
                    vsize: u32_at(bytes, s + 8)?,
                    va: u32_at(bytes, s + 12)?,
                    raw_size: u32_at(bytes, s + 16)?,
                    raw_ptr: u32_at(bytes, s + 20)?,
                })
            })
            .collect::<Result<Vec<_>, String>>()?;
        Ok(Self {
            bytes,
            sections,
            image_base,
            dirs,
        })
    }

    fn rva_to_off(&self, rva: u32) -> Result<usize, String> {
        self.sections
            .iter()
            .find(|s| rva >= s.va && rva - s.va < s.vsize.max(s.raw_size))
            .and_then(|s| {
                let delta = rva - s.va;
                let off = s.raw_ptr.checked_add(delta)?;
                (delta < s.raw_size).then_some(off as usize)
            })
            .ok_or_else(|| format!("RVA {rva:#x} is outside every section"))
    }

    fn cstr_at_rva(&self, rva: u32) -> Result<String, String> {
        let off = self.rva_to_off(rva)?;
        let tail = self
            .bytes
            .get(off..)
            .ok_or("name offset past end of file")?;
        let len = tail
            .iter()
            .take(512)
            .position(|&c| c == 0)
            .ok_or_else(|| format!("unterminated DLL name at RVA {rva:#x}"))?;
        String::from_utf8(tail[..len].to_vec()).map_err(|_| "non-UTF-8 DLL name".into())
    }

    fn dir(&self, idx: usize) -> Option<(u32, u32)> {
        self.dirs
            .get(idx)
            .copied()
            .filter(|&(rva, size)| rva != 0 && size != 0)
    }

    fn load_time(&self) -> Result<Vec<String>, String> {
        let Some((rva, _)) = self.dir(IMAGE_DIRECTORY_ENTRY_IMPORT) else {
            return Ok(Vec::new());
        };
        let base = self.rva_to_off(rva)?;
        let mut out = Vec::new();
        // IMAGE_IMPORT_DESCRIPTOR: 20 bytes, Name RVA at +12; an all-zero entry terminates.
        for i in 0..MAX_DESCRIPTORS {
            let d = base + i * 20;
            let name = u32_at(self.bytes, d + 12)?;
            if name == 0 {
                return Ok(out);
            }
            out.push(self.cstr_at_rva(name)?);
        }
        Err("import directory is not terminated".into())
    }

    fn delay_load(&self) -> Result<Vec<String>, String> {
        let Some((rva, _)) = self.dir(IMAGE_DIRECTORY_ENTRY_DELAY_IMPORT) else {
            return Ok(Vec::new());
        };
        let base = self.rva_to_off(rva)?;
        let mut out = Vec::new();
        // IMAGE_DELAYLOAD_DESCRIPTOR: 32 bytes, Attributes at +0, DllNameRVA at +4. Attribute bit 0
        // set = RVAs (every linker since VC7); clear = the old VC6 format with VAs.
        for i in 0..MAX_DESCRIPTORS {
            let d = base + i * 32;
            let attrs = u32_at(self.bytes, d)?;
            let name = u32_at(self.bytes, d + 4)?;
            if name == 0 {
                return Ok(out);
            }
            let name_rva = if attrs & 1 == 1 {
                name
            } else {
                u32::try_from(u64::from(name).wrapping_sub(self.image_base))
                    .map_err(|_| "bad VA in VC6-style delay descriptor")?
            };
            out.push(self.cstr_at_rva(name_rva)?);
        }
        Err("delay-import directory is not terminated".into())
    }
}

/// Parses the import tables of an in-memory PE image.
pub fn imports_of(bytes: &[u8]) -> Result<Imports, String> {
    let img = Image::parse(bytes)?;
    Ok(Imports {
        load_time: img.load_time()?,
        delay_load: img.delay_load()?,
    })
}

const IMAGE_DIRECTORY_ENTRY_RESOURCE: usize = 2;
const RT_MANIFEST: u32 = 24;
/// Entries walked per resource directory level (a real image has a handful).
const MAX_RES_ENTRIES: usize = 1024;

impl Image<'_> {
    /// `(id, offset, is_dir)` of the entries of the resource directory at `off` (relative to the
    /// resource section start `base`). Named entries come first and get id `u32::MAX`.
    fn res_entries(&self, base: usize, off: usize) -> Result<Vec<(u32, usize, bool)>, String> {
        let d = base.checked_add(off).ok_or("resource offset overflow")?;
        let named = usize::from(u16_at(self.bytes, d + 12)?);
        let ids = usize::from(u16_at(self.bytes, d + 14)?);
        if named + ids > MAX_RES_ENTRIES {
            return Err("resource directory too large".into());
        }
        (0..named + ids)
            .map(|i| {
                let e = d + 16 + i * 8;
                let name = u32_at(self.bytes, e)?;
                let to = u32_at(self.bytes, e + 4)?;
                let id = if name & 0x8000_0000 != 0 {
                    u32::MAX
                } else {
                    name
                };
                Ok((id, (to & 0x7fff_ffff) as usize, to & 0x8000_0000 != 0))
            })
            .collect()
    }

    /// The bytes of every `RT_MANIFEST` resource (any name, any language).
    fn manifests(&self) -> Result<Vec<Vec<u8>>, String> {
        let Some((rva, _)) = self.dir(IMAGE_DIRECTORY_ENTRY_RESOURCE) else {
            return Ok(Vec::new());
        };
        let base = self.rva_to_off(rva)?;
        let mut out = Vec::new();
        for (ty, off, is_dir) in self.res_entries(base, 0)? {
            if ty != RT_MANIFEST || !is_dir {
                continue;
            }
            for (_, name_off, name_dir) in self.res_entries(base, off)? {
                if !name_dir {
                    continue;
                }
                for (_, data_off, lang_dir) in self.res_entries(base, name_off)? {
                    if lang_dir {
                        continue;
                    }
                    // IMAGE_RESOURCE_DATA_ENTRY: OffsetToData (an RVA), Size.
                    let e = base + data_off;
                    let data_rva = u32_at(self.bytes, e)?;
                    let size = u32_at(self.bytes, e + 4)? as usize;
                    let start = self.rva_to_off(data_rva)?;
                    let bytes = self
                        .bytes
                        .get(start..start.checked_add(size).ok_or("manifest size overflow")?)
                        .ok_or("manifest resource past end of file")?;
                    out.push(bytes.to_vec());
                }
            }
        }
        Ok(out)
    }
}

/// The embedded application manifests (`RT_MANIFEST` resources) of an in-memory PE image, as
/// text. Empty when the image has none.
pub fn manifests_of(bytes: &[u8]) -> Result<Vec<String>, String> {
    let img = Image::parse(bytes)?;
    Ok(img
        .manifests()?
        .into_iter()
        .map(|b| {
            let b = b.strip_prefix(&[0xef, 0xbb, 0xbf][..]).unwrap_or(&b);
            String::from_utf8_lossy(b).into_owned()
        })
        .collect())
}

/// Reads `path` and returns its embedded manifests.
pub fn manifests_of_file(path: &Path) -> Result<Vec<String>, String> {
    let bytes = std::fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?;
    manifests_of(&bytes).map_err(|e| format!("{}: {e}", path.display()))
}

/// Reads `path` and parses its import tables.
pub fn imports_of_file(path: &Path) -> Result<Imports, String> {
    let bytes = std::fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?;
    imports_of(&bytes).map_err(|e| format!("{}: {e}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(windows)]
    fn lower(v: &[String]) -> Vec<String> {
        v.iter().map(|s| s.to_ascii_lowercase()).collect()
    }

    // The test binary is a PE file only on Windows.
    #[cfg(windows)]
    #[test]
    fn reads_own_test_binary() {
        let exe = std::env::current_exe().unwrap_or_else(|e| panic!("current_exe: {e}"));
        let imp = imports_of_file(&exe).unwrap_or_else(|e| panic!("{e}"));
        let lt = lower(&imp.load_time);
        assert!(
            lt.iter().any(|d| d == "kernel32.dll"),
            "kernel32 missing: {lt:?}"
        );
        // xtask has no build.rs, so it links the default dynamic VC++ runtime (like sr-daemon).
        assert!(
            lt.iter().any(|d| d.starts_with("vcruntime")),
            "vcruntime missing: {lt:?}"
        );
        assert!(
            imp.delay_load.is_empty(),
            "unexpected delay imports: {:?}",
            imp.delay_load
        );
    }

    #[test]
    fn rejects_non_pe() {
        assert!(imports_of(b"").is_err());
        assert!(imports_of(b"MZ").is_err());
        let mut fake = vec![0u8; 0x100];
        fake[0..2].copy_from_slice(b"MZ");
        fake[0x3c] = 0x80;
        assert!(imports_of(&fake).is_err(), "missing PE signature must fail");
        fake[0x80..0x84].copy_from_slice(b"PE\0\0");
        assert!(
            imports_of(&fake).is_err(),
            "zero optional-header magic must fail"
        );
    }

    #[test]
    fn truncated_real_image_fails_cleanly() {
        let exe = std::env::current_exe().unwrap_or_else(|e| panic!("current_exe: {e}"));
        let bytes = std::fs::read(exe).unwrap_or_else(|e| panic!("{e}"));
        for cut in [0x40, 0x200, 0x400] {
            // Either an error or (if the headers fit) a result; never a panic.
            let _ = imports_of(&bytes[..cut.min(bytes.len())]);
        }
    }
}
