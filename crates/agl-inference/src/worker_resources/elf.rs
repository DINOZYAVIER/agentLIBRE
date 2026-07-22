use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fs::{self, File};
use std::os::unix::fs::FileExt as _;
use std::path::{Path, PathBuf};

const ELF_HEADER_BYTES: usize = 64;
const PROGRAM_HEADER_BYTES: usize = 56;
const MAX_PROGRAM_HEADERS: usize = 256;
const MAX_DYNAMIC_ENTRIES: usize = 4096;
const MAX_STRING_TABLE_BYTES: usize = 4 * 1024 * 1024;
const MAX_DEPENDENCY_FILES: usize = 256;

pub(super) fn dependency_closure(
    seeds: &[PathBuf],
    covered_directory: Option<&Path>,
) -> Result<Vec<PathBuf>, String> {
    let pending = seeds
        .iter()
        .cloned()
        .map(|path| PendingElfObject {
            path,
            origin_override: None,
        })
        .collect::<VecDeque<_>>();
    dependency_closure_inner(pending, covered_directory)
}

pub(super) fn dependency_closure_with_pinned_seed(
    pinned_seed: &Path,
    pinned_origin: &Path,
    additional_seeds: &[PathBuf],
    covered_directory: Option<&Path>,
) -> Result<Vec<PathBuf>, String> {
    if !pinned_origin.is_absolute() {
        return Err("pinned ELF origin is not absolute".to_string());
    }
    let mut pending = VecDeque::with_capacity(additional_seeds.len().saturating_add(1));
    pending.push_back(PendingElfObject {
        path: pinned_seed.to_path_buf(),
        origin_override: Some(pinned_origin.to_path_buf()),
    });
    pending.extend(
        additional_seeds
            .iter()
            .cloned()
            .map(|path| PendingElfObject {
                path,
                origin_override: None,
            }),
    );
    dependency_closure_inner(pending, covered_directory)
}

pub(super) fn native_bundle_search_directory(
    worker_elf: &Path,
    worker_origin: &Path,
    bundle_base: &Path,
) -> Result<PathBuf, String> {
    let dynamic = read_dynamic_info(worker_elf, Some(worker_origin))?;
    let mut matches = dynamic
        .search_paths
        .into_iter()
        .filter(|path| path.parent() == Some(bundle_base))
        .collect::<BTreeSet<_>>();
    match matches.len() {
        1 => Ok(matches.pop_first().expect("one exact bundle search path")),
        0 => Err(
            "worker ELF does not name one content-addressed native bundle search directory"
                .to_string(),
        ),
        _ => Err(
            "worker ELF names more than one content-addressed native bundle search directory"
                .to_string(),
        ),
    }
}

struct PendingElfObject {
    path: PathBuf,
    origin_override: Option<PathBuf>,
}

fn dependency_closure_inner(
    mut pending: VecDeque<PendingElfObject>,
    covered_directory: Option<&Path>,
) -> Result<Vec<PathBuf>, String> {
    let mut visited = BTreeSet::new();
    let mut result = BTreeSet::new();
    let mut resolved_names = BTreeMap::<String, PathBuf>::new();
    while let Some(object) = pending.pop_front() {
        let path = match &object.origin_override {
            Some(_) => object.path,
            None => fs::canonicalize(&object.path).map_err(|error| {
                format!(
                    "failed to resolve ELF object {}: {error}",
                    object.path.display()
                )
            })?,
        };
        if !visited.insert(path.clone()) {
            continue;
        }
        if visited.len() > MAX_DEPENDENCY_FILES {
            return Err(format!(
                "ELF dependency closure exceeds {MAX_DEPENDENCY_FILES} files"
            ));
        }
        let dynamic = read_dynamic_info(&path, object.origin_override.as_deref())?;
        if let Some(interpreter) = &dynamic.interpreter {
            if let Some(name) = interpreter.file_name().and_then(|name| name.to_str()) {
                resolved_names
                    .entry(name.to_owned())
                    .or_insert_with(|| interpreter.clone());
            }
            if !covered_directory.is_some_and(|root| interpreter.starts_with(root)) {
                result.insert(interpreter.clone());
            }
            pending.push_back(PendingElfObject {
                path: interpreter.clone(),
                origin_override: None,
            });
        }
        for needed in dynamic.needed {
            // glibc reuses the first loaded object for a SONAME. Preserve that
            // deterministic load-order behavior across the worker and plugin
            // seed sequence instead of resolving a later duplicate anew.
            let dependency = if let Some(prior) = resolved_names.get(&needed) {
                prior.clone()
            } else {
                let dependency = resolve_needed(&path, &needed, &dynamic.search_paths)?;
                resolved_names.insert(needed.clone(), dependency.clone());
                dependency
            };
            if !covered_directory.is_some_and(|root| dependency.starts_with(root)) {
                result.insert(dependency.clone());
            }
            pending.push_back(PendingElfObject {
                path: dependency,
                origin_override: None,
            });
        }
    }
    Ok(result.into_iter().collect())
}

struct DynamicInfo {
    needed: Vec<String>,
    search_paths: Vec<PathBuf>,
    interpreter: Option<PathBuf>,
}

#[derive(Clone, Copy)]
struct LoadSegment {
    offset: u64,
    virtual_address: u64,
    file_size: u64,
}

fn read_dynamic_info(path: &Path, origin_override: Option<&Path>) -> Result<DynamicInfo, String> {
    let file = File::open(path)
        .map_err(|error| format!("failed to open ELF object {}: {error}", path.display()))?;
    let mut header = [0_u8; ELF_HEADER_BYTES];
    read_exact_at(&file, &mut header, 0, path)?;
    if &header[..4] != b"\x7fELF" || header[4] != 2 || header[5] != 1 {
        return Err(format!(
            "runtime object {} is not a 64-bit little-endian ELF file",
            path.display()
        ));
    }
    let program_offset = le_u64(&header, 32)?;
    let entry_size = usize::from(le_u16(&header, 54)?);
    let entry_count = usize::from(le_u16(&header, 56)?);
    if entry_size != PROGRAM_HEADER_BYTES || entry_count > MAX_PROGRAM_HEADERS {
        return Err(format!(
            "ELF program-header table is outside its bound: {}",
            path.display()
        ));
    }

    let mut loads = Vec::new();
    let mut dynamic = None;
    let mut interpreter = None;
    for index in 0..entry_count {
        let entry_offset = index
            .checked_mul(entry_size)
            .and_then(|value| u64::try_from(value).ok())
            .ok_or_else(|| "ELF program-header offset overflow".to_string())?;
        let offset = program_offset
            .checked_add(entry_offset)
            .ok_or_else(|| "ELF program-header offset overflow".to_string())?;
        let mut entry = [0_u8; PROGRAM_HEADER_BYTES];
        read_exact_at(&file, &mut entry, offset, path)?;
        match le_u32(&entry, 0)? {
            1 => loads.push(LoadSegment {
                offset: le_u64(&entry, 8)?,
                virtual_address: le_u64(&entry, 16)?,
                file_size: le_u64(&entry, 32)?,
            }),
            2 => dynamic = Some((le_u64(&entry, 8)?, le_u64(&entry, 32)?)),
            3 => {
                if interpreter.is_some() {
                    return Err("ELF contains more than one PT_INTERP header".to_string());
                }
                interpreter = Some((le_u64(&entry, 8)?, le_u64(&entry, 32)?));
            }
            _ => {}
        }
    }
    let interpreter = interpreter
        .map(|(offset, size)| read_interpreter(&file, offset, size, path))
        .transpose()?;
    let Some((dynamic_offset, dynamic_size)) = dynamic else {
        return Ok(DynamicInfo {
            needed: Vec::new(),
            search_paths: Vec::new(),
            interpreter,
        });
    };
    if dynamic_size % 16 != 0 {
        return Err("ELF dynamic table has a partial entry".to_string());
    }
    let count = usize::try_from(dynamic_size / 16)
        .map_err(|_| "ELF dynamic table size overflow".to_string())?;
    if count > MAX_DYNAMIC_ENTRIES {
        return Err(format!(
            "ELF dynamic table is outside its bound: {}",
            path.display()
        ));
    }
    let mut needed_offsets = Vec::new();
    let mut runpath_offset = None;
    let mut rpath_offset = None;
    let mut string_address = None;
    let mut string_size = None;
    let mut flags_1 = 0_u64;
    for index in 0..count {
        let mut entry = [0_u8; 16];
        let entry_offset = index
            .checked_mul(16)
            .and_then(|value| u64::try_from(value).ok())
            .and_then(|value| dynamic_offset.checked_add(value))
            .ok_or_else(|| "ELF dynamic-entry offset overflow".to_string())?;
        read_exact_at(&file, &mut entry, entry_offset, path)?;
        let tag = le_i64(&entry, 0)?;
        let value = le_u64(&entry, 8)?;
        match tag {
            0 => break,
            1 => needed_offsets.push(value),
            5 => string_address = Some(value),
            10 => string_size = Some(value),
            15 => rpath_offset = Some(value),
            29 => runpath_offset = Some(value),
            0x6fff_fffb => flags_1 = value,
            _ => {}
        }
    }
    if needed_offsets.is_empty() && runpath_offset.is_none() && rpath_offset.is_none() {
        return Ok(DynamicInfo {
            needed: Vec::new(),
            search_paths: Vec::new(),
            interpreter,
        });
    }
    let string_address =
        string_address.ok_or_else(|| "ELF dynamic table has no DT_STRTAB".to_string())?;
    let string_size = usize::try_from(
        string_size.ok_or_else(|| "ELF dynamic table has no DT_STRSZ".to_string())?,
    )
    .map_err(|_| "ELF string table size overflow".to_string())?;
    if string_size == 0 || string_size > MAX_STRING_TABLE_BYTES {
        return Err(format!(
            "ELF string table is outside its bound: {}",
            path.display()
        ));
    }
    let string_offset = loads
        .iter()
        .find_map(|segment| {
            let relative = string_address.checked_sub(segment.virtual_address)?;
            (relative < segment.file_size)
                .then(|| segment.offset.checked_add(relative))
                .flatten()
        })
        .ok_or_else(|| "ELF string table is not backed by a load segment".to_string())?;
    let mut strings = vec![0_u8; string_size];
    read_exact_at(&file, &mut strings, string_offset, path)?;
    let needed = needed_offsets
        .into_iter()
        .map(|offset| dynamic_string(&strings, offset, path))
        .collect::<Result<Vec<_>, _>>()?;
    let search = runpath_offset
        .or(rpath_offset)
        .map(|offset| dynamic_string(&strings, offset, path))
        .transpose()?
        .unwrap_or_default();
    let origin = origin_override
        .or_else(|| path.parent())
        .ok_or_else(|| "ELF object has no parent directory".to_string())?;
    let nodeflib = flags_1 & 0x800 != 0;
    let loader_directory = interpreter.as_deref().and_then(Path::parent);
    let search_paths = expand_search_paths(&search, origin, loader_directory, nodeflib)?;
    Ok(DynamicInfo {
        needed,
        search_paths,
        interpreter,
    })
}

fn read_interpreter(file: &File, offset: u64, size: u64, path: &Path) -> Result<PathBuf, String> {
    const MAX_INTERPRETER_BYTES: usize = 4096;
    let size = usize::try_from(size).map_err(|_| "ELF interpreter size overflow".to_string())?;
    if !(2..=MAX_INTERPRETER_BYTES).contains(&size) {
        return Err(format!(
            "ELF interpreter path is outside its bound: {}",
            path.display()
        ));
    }
    let mut bytes = vec![0_u8; size];
    read_exact_at(file, &mut bytes, offset, path)?;
    if bytes.pop() != Some(0) || bytes.contains(&0) {
        return Err(format!(
            "ELF interpreter path is not one terminated string: {}",
            path.display()
        ));
    }
    let value = std::str::from_utf8(&bytes)
        .map_err(|_| format!("ELF interpreter path is not UTF-8: {}", path.display()))?;
    let interpreter = Path::new(value);
    if !interpreter.is_absolute() {
        return Err(format!(
            "ELF interpreter path is not absolute: {}",
            path.display()
        ));
    }
    fs::canonicalize(interpreter).map_err(|error| {
        format!(
            "failed to resolve ELF interpreter {} for {}: {error}",
            interpreter.display(),
            path.display()
        )
    })
}

fn expand_search_paths(
    search: &str,
    origin: &Path,
    loader_directory: Option<&Path>,
    nodeflib: bool,
) -> Result<Vec<PathBuf>, String> {
    let mut search_paths = Vec::new();
    if !search.is_empty() {
        for value in search.split(':') {
            if value.is_empty() {
                return Err("ELF search path contains a current-directory entry".to_string());
            }
            let expanded = value
                .replace("${ORIGIN}", &origin.to_string_lossy())
                .replace("$ORIGIN", &origin.to_string_lossy());
            if expanded.contains('$') {
                return Err(format!(
                    "ELF search path contains an unsupported token: {value}"
                ));
            }
            let expanded = PathBuf::from(expanded);
            if !expanded.is_absolute() {
                return Err(format!(
                    "ELF search path is relative and therefore ambiguous: {value}"
                ));
            }
            search_paths.push(expanded);
        }
    }
    if !nodeflib {
        if let Some(loader_directory) = loader_directory {
            search_paths.push(loader_directory.to_path_buf());
        }
        search_paths.extend(default_library_directories());
    }
    Ok(search_paths)
}

fn resolve_needed(object: &Path, name: &str, search_paths: &[PathBuf]) -> Result<PathBuf, String> {
    if name.is_empty() || name.contains('/') || name.as_bytes().contains(&0) {
        return Err(format!(
            "ELF object {} has an unsafe DT_NEEDED name",
            object.display()
        ));
    }
    for directory in search_paths {
        let candidate = directory.join(name);
        let Ok(resolved) = fs::canonicalize(&candidate) else {
            continue;
        };
        if fs::metadata(&resolved).is_ok_and(|metadata| metadata.is_file()) {
            return Ok(resolved);
        }
    }
    Err(format!(
        "failed to resolve DT_NEEDED {name:?} for {} without loader search ambiguity",
        object.display()
    ))
}

fn default_library_directories() -> Vec<PathBuf> {
    let mut paths = vec![
        PathBuf::from("/lib"),
        PathBuf::from("/lib64"),
        PathBuf::from("/usr/lib"),
        PathBuf::from("/usr/lib64"),
    ];
    #[cfg(target_arch = "x86_64")]
    paths.extend([
        PathBuf::from("/lib/x86_64-linux-gnu"),
        PathBuf::from("/usr/lib/x86_64-linux-gnu"),
    ]);
    #[cfg(target_arch = "aarch64")]
    paths.extend([
        PathBuf::from("/lib/aarch64-linux-gnu"),
        PathBuf::from("/usr/lib/aarch64-linux-gnu"),
    ]);
    paths
}

fn dynamic_string(strings: &[u8], offset: u64, path: &Path) -> Result<String, String> {
    let offset = usize::try_from(offset).map_err(|_| "ELF string offset overflow".to_string())?;
    let tail = strings
        .get(offset..)
        .ok_or_else(|| "ELF string offset is outside DT_STRSZ".to_string())?;
    let end = tail
        .iter()
        .position(|byte| *byte == 0)
        .ok_or_else(|| "ELF dynamic string is not terminated".to_string())?;
    let value = std::str::from_utf8(&tail[..end])
        .map_err(|_| format!("ELF dynamic string is not UTF-8: {}", path.display()))?;
    Ok(value.to_owned())
}

fn read_exact_at(file: &File, buffer: &mut [u8], offset: u64, path: &Path) -> Result<(), String> {
    file.read_exact_at(buffer, offset)
        .map_err(|error| format!("failed to read ELF object {}: {error}", path.display()))
}

fn le_u16(bytes: &[u8], offset: usize) -> Result<u16, String> {
    Ok(u16::from_le_bytes(
        slice(bytes, offset, 2)?.try_into().expect("exact slice"),
    ))
}
fn le_u32(bytes: &[u8], offset: usize) -> Result<u32, String> {
    Ok(u32::from_le_bytes(
        slice(bytes, offset, 4)?.try_into().expect("exact slice"),
    ))
}
fn le_u64(bytes: &[u8], offset: usize) -> Result<u64, String> {
    Ok(u64::from_le_bytes(
        slice(bytes, offset, 8)?.try_into().expect("exact slice"),
    ))
}
fn le_i64(bytes: &[u8], offset: usize) -> Result<i64, String> {
    Ok(i64::from_le_bytes(
        slice(bytes, offset, 8)?.try_into().expect("exact slice"),
    ))
}
fn slice(bytes: &[u8], offset: usize, length: usize) -> Result<&[u8], String> {
    let end = offset
        .checked_add(length)
        .ok_or_else(|| "ELF field offset overflow".to_string())?;
    bytes
        .get(offset..end)
        .ok_or_else(|| "ELF field is outside its structure".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn search_paths_reject_current_directory_relative_and_unknown_tokens() {
        let origin = Path::new("/immutable/origin");
        assert!(expand_search_paths("$ORIGIN:", origin, None, false).is_err());
        assert!(expand_search_paths("relative", origin, None, false).is_err());
        assert!(expand_search_paths("$LIB", origin, None, false).is_err());
        assert_eq!(
            expand_search_paths("$ORIGIN", origin, None, true).unwrap(),
            vec![origin]
        );
    }

    #[test]
    fn malformed_offsets_fail_without_wrapping() {
        assert!(slice(&[0; 8], usize::MAX, 2).is_err());
        assert!(slice(&[0; 8], 7, 2).is_err());
    }
}
