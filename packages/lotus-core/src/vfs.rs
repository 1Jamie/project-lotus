use std::collections::HashMap;
use std::num::NonZeroUsize;
use std::path::PathBuf;
use std::{env, fs};

use aes_gcm::{
    aead::{Aead, KeyInit},
    Aes256Gcm, Nonce,
};
use lru::LruCache;
use once_cell::sync::Lazy;
use servo::resources::{self, Resource};

pub(crate) struct ByteLimitedLruCache {
    cache: LruCache<PathBuf, (Vec<u8>, String)>,
    current_bytes: usize,
    max_bytes: usize,
}

impl ByteLimitedLruCache {
    pub(crate) fn new(max_bytes: usize) -> Self {
        Self {
            cache: LruCache::new(NonZeroUsize::new(1000).unwrap()), // Cap at 1000 items as secondary guard
            current_bytes: 0,
            max_bytes,
        }
    }

    pub(crate) fn get(&mut self, key: &PathBuf) -> Option<&(Vec<u8>, String)> {
        self.cache.get(key)
    }

    pub(crate) fn put(&mut self, key: PathBuf, val: (Vec<u8>, String)) {
        let size = val.0.len();
        // If the new item is larger than the total cache size, don't cache it
        if size > self.max_bytes {
            return;
        }

        while self.current_bytes + size > self.max_bytes {
            if let Some((_, (old_data, _))) = self.cache.pop_lru() {
                self.current_bytes -= old_data.len();
            } else {
                break;
            }
        }

        self.current_bytes += size;
        self.cache.put(key, val);
    }
}

pub(crate) struct AutonomousKeyDeriver;

impl AutonomousKeyDeriver {
    pub(crate) fn derive_key() -> Option<[u8; 32]> {
        let shard1 = b"LotusMasterFrameworkShard_v1_2026";
        let shard2 = Self::read_shard_from_binary("LOTUS_APP_S1")?;
        let shard3 = Self::read_shard_from_binary("LOTUS_APP_S2")?;

        if shard2.len() != 32 || shard3.len() != 32 {
            eprintln!("[DEBUG] Invalid shard length: s2={}, s3={}", shard2.len(), shard3.len());
            return None;
        }

        let mut key = [0u8; 32];
        for i in 0..32 {
            key[i] = shard2[i] ^ shard3[i] ^ shard1[i % shard1.len()];
        }
        eprintln!("[DEBUG] Key derivation successful.");
        Some(key)
    }

    pub(crate) fn has_encrypted_sections() -> bool {
        Self::read_shard_from_binary("LOTUS_APP_S1").is_some()
            || Self::read_shard_from_binary("LOTUS_APP_S2").is_some()
            || Self::read_shard_from_binary("LOTUS_VFS").is_some()
    }

    pub(crate) fn read_shard_from_binary(name: &str) -> Option<Vec<u8>> {
        eprintln!("[DEBUG] Reading shard: {}", name);
        #[cfg(target_os = "windows")]
        {
            use windows_sys::Win32::System::LibraryLoader::{GetModuleHandleW, FindResourceW, LoadResource, LockResource, SizeofResource};

            unsafe {
                let module = GetModuleHandleW(std::ptr::null());
                if module == 0 { return None; }

                // Convert name to UTF-16
                let name_u16: Vec<u16> = name.encode_utf16().chain(std::iter::once(0)).collect();
                // RT_RCDATA is 10
                let rt_rcdata = 10 as *const u16;

                let res = FindResourceW(module, name_u16.as_ptr(), rt_rcdata);
                if res == 0 { return None; }

                let size = SizeofResource(module, res);
                if size == 0 { return None; }

                let handle = LoadResource(module, res);
                if handle.is_null() { return None; }

                let data_ptr = LockResource(handle);
                if data_ptr.is_null() { return None; }

                let mut data = vec![0u8; size as usize];
                std::ptr::copy_nonoverlapping(data_ptr as *const u8, data.as_mut_ptr(), size as usize);
                Some(data)
            }
        }

        #[cfg(target_os = "linux")]
        {
            use object::{Object, ObjectSection};
            use std::fs::File;
            let file = match File::open("/proc/self/exe") {
                Ok(f) => f,
                Err(e) => {
                    eprintln!("[DEBUG] Failed to open /proc/self/exe: {}", e);
                    return None;
                }
            };
            let mmap = unsafe {
                match memmap2::Mmap::map(&file) {
                    Ok(m) => m,
                    Err(e) => {
                        eprintln!("[DEBUG] Failed to map /proc/self/exe: {}", e);
                        return None;
                    }
                }
            };
            let obj_file = match object::File::parse(&*mmap) {
                Ok(o) => o,
                Err(e) => {
                    eprintln!("[DEBUG] Failed to parse ELF: {}", e);
                    return None;
                }
            };
            let section = match obj_file.section_by_name(name) {
                Some(s) => s,
                None => {
                    eprintln!("[DEBUG] Section not found: {}", name);
                    return None;
                }
            };
            eprintln!("[DEBUG] Found section {}, size: {}", name, section.size());
            section.data().ok().map(|d| d.to_vec())
        }

        #[cfg(not(any(target_os = "windows", target_os = "linux")))]
        {
            None
        }
    }
}

pub(crate) struct EncryptedVfs {
    data: Vec<u8>,
    index: HashMap<String, (usize, usize)>, // path -> (offset, size)
    data_offset: usize,
    cipher: Aes256Gcm,
}

impl EncryptedVfs {
    pub(crate) fn init() -> Result<Option<Self>, String> {
        if !AutonomousKeyDeriver::has_encrypted_sections() {
            return Ok(None);
        }

        eprintln!("[DEBUG] Initializing EncryptedVfs (detected encrypted sections)...");
        let key = match AutonomousKeyDeriver::derive_key() {
            Some(k) => k,
            None => {
                return Err("Failed to derive VFS master key: shards missing or invalid length".to_string());
            }
        };
        let cipher = Aes256Gcm::new(&key.into());

        let vfs_data = match AutonomousKeyDeriver::read_shard_from_binary("LOTUS_VFS") {
            Some(d) => d,
            None => {
                return Err("Failed to read LOTUS_VFS section/resource from executable".to_string());
            }
        };

        if vfs_data.len() < 12 || &vfs_data[0..8] != b"LOTUSVFS" {
            return Err("Invalid VFS magic or size (corrupted VFS payload)".to_string());
        }

        let index_size = u32::from_le_bytes(vfs_data[8..12].try_into().unwrap()) as usize;
        if vfs_data.len() < 12 + index_size {
            return Err("Corrupted VFS payload: index size exceeds binary data length".to_string());
        }

        eprintln!("[DEBUG] VFS Index size: {}", index_size);
        let index_json = &vfs_data[12..12+index_size];
        let index: HashMap<String, (usize, usize)> = match serde_json::from_slice(index_json) {
            Ok(idx) => idx,
            Err(e) => {
                return Err(format!("Failed to parse VFS index JSON: {}", e));
            }
        };

        let data_offset = 12 + index_size;
        eprintln!("[DEBUG] VFS initialized with {} entries.", index.len());

        Ok(Some(Self {
            data: vfs_data,
            index,
            data_offset,
            cipher,
        }))
    }

    pub(crate) fn read_file(&self, path: &str) -> Option<Vec<u8>> {
        let (offset, size) = self.index.get(path)?;
        let encrypted_data = &self.data[self.data_offset + offset .. self.data_offset + offset + size];

        // Nonce is prepended to the data block in our VFS format
        if encrypted_data.len() < 12 { return None; }
        let (nonce_bytes, ciphertext) = encrypted_data.split_at(12);
        let nonce = Nonce::from_slice(nonce_bytes);

        self.cipher.decrypt(nonce, ciphertext).ok()
    }
}

pub(crate) struct ResourceReader;

impl resources::ResourceReaderMethods for ResourceReader {
    fn read(&self, file: Resource) -> Vec<u8> {
        let mut path = resources_dir_path().clone();
        path.push(file.filename());
        match fs::read(&path) {
            Ok(data) => data,
            Err(e) => {
                eprintln!("Rust Warning: Missing resource: {:?} ({})", path, e);
                Vec::new()
            }
        }
    }
    fn sandbox_access_files_dirs(&self) -> Vec<PathBuf> {
        vec![resources_dir_path().clone()]
    }
    fn sandbox_access_files(&self) -> Vec<PathBuf> {
        vec![]
    }
}

static RESOURCE_READER: ResourceReader = ResourceReader;
servo::submit_resource_reader!(&RESOURCE_READER);

static RESOURCES_DIR: Lazy<PathBuf> = Lazy::new(|| {
    // 1. Check explicit environment variable
    if let Ok(res_env) = env::var("LOTUS_RESOURCES_PATH") {
        let p = PathBuf::from(res_env);
        if p.exists() {
            return p;
        }
    }

    // 2. Check current working directory
    let cur = env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let p = cur.join("resources");
    if p.exists() {
        return p;
    }

    // 3. Search upwards through ancestor directories (up to 5 levels)
    let mut search = cur.clone();
    for _ in 0..5 {
        if search.pop() {
            let candidate = search.join("resources");
            if candidate.join("debugger.js").exists() || candidate.join("rippy.png").exists() {
                return candidate;
            }
        } else {
            break;
        }
    }

    // 4. Default fallback
    cur.join("resources")
});

fn resources_dir_path() -> &'static PathBuf {
    &RESOURCES_DIR
}
