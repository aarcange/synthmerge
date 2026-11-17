// SPDX-License-Identifier: GPL-3.0-or-later OR AGPL-3.0-or-later
// Copyright (C) 2026  Red Hat, Inc.

use anyhow::Result;
use lmdb::Transaction;
use sha2::{Digest, Sha256};
use std::path::Path;

pub trait ApiCache {
    fn cache_response(&self, cache_key: &str, response: String) -> Result<()>;
    fn get_cached_response(&self, cache_key: &str) -> Option<String>;
    fn create_from_path(path: &str, overwrite: bool) -> Result<Self>
    where
        Self: std::marker::Sized;
}

pub trait PatchLocatorCache {
    fn cache_response(&self, cache_key: &str, response: String) -> Result<()>;
    fn get_cached_response(&self, cache_key: &str) -> Option<String>;
    fn cache_minus_lines(&self, cache_key: &str, response: bool) -> Result<()>;
    fn get_cached_minus_lines(&self, cache_key: &str) -> Option<bool>;
    fn create_from_path(path: &str, overwrite: bool) -> Result<Self>
    where
        Self: std::marker::Sized;
}

pub struct LmdbCacheImpl {
    pub(crate) lmdb_env: lmdb::Environment,
    pub(crate) lmdb_db: lmdb::Database,
    overwrite: bool,
}

impl LmdbCacheImpl {
    pub const API_CACHE_DB: &'static str = "api_cache_v1";
    pub const PATCH_LOCATOR_CACHE_DB: &'static str = "patch_locator_cache_v1";
    pub const DB_NAMES: &'static [&'static str] =
        &[Self::API_CACHE_DB, Self::PATCH_LOCATOR_CACHE_DB];

    pub fn new(lmdb_env: lmdb::Environment, lmdb_db: lmdb::Database, overwrite: bool) -> Self {
        LmdbCacheImpl {
            lmdb_env,
            lmdb_db,
            overwrite,
        }
    }

    pub fn get_cache_key<S: AsRef<[u8]>>(&self, strings: &[S]) -> String {
        let mut hasher = Sha256::new();
        for s in strings {
            let data = s.as_ref();
            hasher.update(data.len().to_string());
            hasher.update(data);
        }
        let hash = hasher.finalize();
        hash.iter().map(|b| format!("{:02x}", b)).collect()
    }

    fn cache_response(&self, cache_key: &str, response: String, compress: bool) -> Result<()> {
        let data = if compress {
            zstd::encode_all(response.as_bytes(), 0).expect("Failed to compress response")
        } else {
            response.into_bytes()
        };
        let env = &self.lmdb_env;
        let mut txn = env
            .begin_rw_txn()
            .expect("Failed to start read transaction");
        txn.put(
            self.lmdb_db,
            &cache_key.to_string(),
            &data,
            lmdb::WriteFlags::empty(),
        )
        .expect("Failed to cache response");
        txn.commit().expect("Failed to commit transaction");
        Ok(())
    }

    fn get_cached_response(&self, cache_key: &str, compress: bool) -> Option<String> {
        if self.overwrite {
            return None;
        }
        let env = &self.lmdb_env;
        let txn = env
            .begin_ro_txn()
            .expect("Failed to start read transaction");
        txn.get(self.lmdb_db, &cache_key.to_string())
            .ok()
            .and_then(|v| {
                if compress {
                    zstd::decode_all(v).ok()
                } else {
                    Some(v.to_vec())
                }
            })
            .map(|data| String::from_utf8_lossy(&data).to_string())
    }

    pub(crate) fn create_env_internal(path: &str, readonly: bool) -> Result<lmdb::Environment> {
        let dir = Path::new(path);
        let mut flags = lmdb::EnvironmentFlags::NO_META_SYNC | lmdb::EnvironmentFlags::NO_TLS;
        if readonly {
            flags |= lmdb::EnvironmentFlags::READ_ONLY;
        } else if !dir.exists() {
            std::fs::create_dir_all(dir).expect("Failed to create cache directory");
        }

        let mut builder = lmdb::Environment::new();
        builder
            .set_max_dbs(Self::DB_NAMES.len() as u32)
            .set_map_size(32 * 1024 * 1024 * 1024)
            .set_flags(flags);

        let env = if readonly {
            builder.open(dir).expect("open readonly lmdb env")
        } else {
            builder
                .open_with_permissions(dir, 0o600)
                .expect("open lmdb env")
        };
        Ok(env)
    }

    fn create_env(path: &str) -> Result<lmdb::Environment> {
        Self::create_env_internal(path, false)
    }

    fn create_db(env: &lmdb::Environment, name: Option<&str>) -> Result<lmdb::Database> {
        let db = env
            .create_db(name, lmdb::DatabaseFlags::empty())
            .expect("open index db");
        Ok(db)
    }

    fn create_from_path(name: Option<&str>, path: &str, overwrite: bool) -> Result<Self> {
        let expanded = shellexpand::full(path)?;
        let env = Self::create_env(&expanded)?;
        let db = Self::create_db(&env, name)?;
        Ok(Self::new(env, db, overwrite))
    }
}

impl ApiCache for LmdbCacheImpl {
    fn cache_response(&self, cache_key: &str, response: String) -> Result<()> {
        self.cache_response(cache_key, response, true)
    }

    fn get_cached_response(&self, cache_key: &str) -> Option<String> {
        self.get_cached_response(cache_key, true)
    }

    fn create_from_path(path: &str, overwrite: bool) -> Result<Self> {
        Self::create_from_path(Some(Self::API_CACHE_DB), path, overwrite)
    }
}

impl PatchLocatorCache for LmdbCacheImpl {
    fn cache_response(&self, cache_key: &str, response: String) -> Result<()> {
        self.cache_response(cache_key, response, false)
    }

    fn get_cached_response(&self, cache_key: &str) -> Option<String> {
        self.get_cached_response(cache_key, false)
    }

    fn cache_minus_lines(&self, cache_key: &str, response: bool) -> Result<()> {
        self.cache_response(
            cache_key,
            if response { "1" } else { "0" }.to_string(),
            false,
        )
    }

    fn get_cached_minus_lines(&self, cache_key: &str) -> Option<bool> {
        self.get_cached_response(cache_key, false).map(|x| x == "1")
    }

    fn create_from_path(path: &str, overwrite: bool) -> Result<Self> {
        Self::create_from_path(Some(Self::PATCH_LOCATOR_CACHE_DB), path, overwrite)
    }
}

// Local Variables:
// rust-format-on-save: t
// End:
