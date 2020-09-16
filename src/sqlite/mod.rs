use std::borrow::Cow;
use std::collections::BTreeMap;
use std::str::FromStr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_mutex::Mutex;
use async_stream::try_stream;
use async_trait::async_trait;
use futures_util::stream::StreamExt;

use sqlx::{
    sqlite::{Sqlite, SqliteConnectOptions, SqlitePool, SqlitePoolOptions, SqliteRow},
    Done, Row,
};

use super::db_utils::{
    expiry_timestamp, extend_query, hash_lock_info, QueryParams, QueryPrepare, Scan, PAGE_SIZE,
};
use super::error::Result as KvResult;
use super::keys::store_key::StoreKey;
use super::options::IntoOptions;
use super::store::{KeyCache, KvProvisionSpec, KvStore, LockToken, ScanToken};
use super::types::{
    EntryEncryptor, KeyId, KvEncTag, KvEntry, KvFetchOptions, KvTag, KvUpdateEntry, ProfileId,
};
use super::wql;
use super::KvProvisionStore;

const LOCK_EXPIRY: i64 = 120000; // 2 minutes
const COUNT_QUERY: &'static str = "SELECT COUNT(*) FROM items i
    WHERE store_key_id = ?1 AND category = ?2
    AND (expiry IS NULL OR expiry > datetime('now'))";
const FETCH_QUERY: &'static str = "SELECT id, value FROM items i
    WHERE store_key_id = ?1 AND category = ?2 AND name = ?3
    AND (expiry IS NULL OR expiry > datetime('now'))";
const INSERT_QUERY: &'static str = "INSERT INTO items(store_key_id, category, name, value, expiry)
    VALUES(?1, ?2, ?3, ?4, ?5)";
const SCAN_QUERY: &'static str = "SELECT id, name, value FROM items i WHERE store_key_id = ?1
    AND category = ?2 AND (expiry IS NULL OR expiry > datetime('now'))";
const TAG_QUERY: &'static str = "SELECT name, value, plaintext FROM items_tags WHERE item_id = ?1";

async fn fetch_row_tags(
    pool: &SqlitePool,
    row_id: i64,
    key: Option<Arc<StoreKey>>,
) -> KvResult<Option<Vec<KvTag>>> {
    let tags = sqlx::query(TAG_QUERY)
        .bind(row_id)
        .try_map(|row: SqliteRow| {
            let name = row.try_get(0)?;
            let value = row.try_get(1)?;
            let plaintext = row.try_get::<i32, _>(2)? != 0;
            Ok(KvEncTag {
                name,
                value,
                plaintext,
            })
        })
        .fetch_all(pool)
        .await?;
    Ok(if tags.is_empty() {
        None
    } else {
        Some(key.decrypt_entry_tags(&tags)?)
    })
}

#[derive(Debug)]
pub struct Lock {
    pub id: i64,
}

#[derive(Debug)]
pub struct KvSqliteOptions<'a> {
    path: Cow<'a, str>,
    options: SqlitePoolOptions,
}

impl<'a> KvSqliteOptions<'a> {
    pub fn new<O>(options: O) -> KvResult<Self>
    where
        O: IntoOptions<'a>,
    {
        let opts = options.into_options()?;
        Ok(Self {
            path: opts.host,
            options: SqlitePoolOptions::default()
                // must maintain at least 1 connection to avoid dropping in-memory database
                .min_connections(1)
                .max_connections(10),
        })
    }

    pub fn in_memory() -> Self {
        Self::new(":memory:").unwrap()
    }
}

#[async_trait]
impl<'a> KvProvisionStore for KvSqliteOptions<'a> {
    type Store = KvSqlite;

    async fn provision_store(self, spec: KvProvisionSpec) -> KvResult<Self::Store> {
        let conn_opts = SqliteConnectOptions::from_str(self.path.as_ref())?.create_if_missing(true);
        let conn_pool = self.options.connect_with(conn_opts).await?;
        let mut conn = conn_pool.acquire().await?;

        sqlx::query(
            r#"
            BEGIN EXCLUSIVE TRANSACTION;

            CREATE TABLE config (
                name TEXT NOT NULL,
                value TEXT,
                PRIMARY KEY(name)
            );
            INSERT INTO config (name, value) VALUES
                ("default_profile", ?1),
                ("wrap_key", ?2),
                ("version", "1");

            CREATE TABLE profiles (
                id INTEGER NOT NULL,
                active_key_id INTEGER NULL,
                name TEXT NOT NULL,
                reference TEXT NULL,
                PRIMARY KEY(id),
                FOREIGN KEY(active_key_id) REFERENCES store_keys(id)
                    ON DELETE SET NULL ON UPDATE CASCADE
            );
            CREATE UNIQUE INDEX ux_profile_name ON profiles(name);

            CREATE TABLE store_keys (
                id INTEGER NOT NULL,
                profile_id INTEGER NOT NULL,
                value BLOB NULL,
                PRIMARY KEY(id),
                FOREIGN KEY(profile_id) REFERENCES profiles(id)
                    ON DELETE CASCADE ON UPDATE CASCADE
            );

            CREATE TABLE keys (
                id INTEGER NOT NULL,
                store_key_id INTEGER NOT NULL,
                category NOT NULL,
                name NOT NULL,
                reference TEXT NULL,
                value BLOB NULL,
                PRIMARY KEY(id),
                FOREIGN KEY(store_key_id) REFERENCES store_keys(id)
                    ON DELETE CASCADE ON UPDATE CASCADE
            );
            CREATE UNIQUE INDEX ux_keys_uniq ON keys(store_key_id, category, name);

            CREATE TABLE items (
                id INTEGER NOT NULL,
                store_key_id INTEGER NOT NULL,
                category NOT NULL,
                name NOT NULL,
                value NOT NULL,
                expiry DATETIME NULL,
                PRIMARY KEY(id),
                FOREIGN KEY(store_key_id) REFERENCES store_keys(id)
                    ON DELETE CASCADE ON UPDATE CASCADE
            );
            CREATE UNIQUE INDEX ux_items_uniq ON items(store_key_id, category, name);

            CREATE TABLE items_tags (
                item_id INTEGER NOT NULL,
                name NOT NULL,
                value NOT NULL,
                plaintext BOOLEAN NOT NULL,
                PRIMARY KEY(name, item_id, plaintext),
                FOREIGN KEY(item_id) REFERENCES items(id)
                    ON DELETE CASCADE ON UPDATE CASCADE
            );
            CREATE INDEX ix_items_tags_item_id ON items_tags(item_id);
            CREATE INDEX ix_items_tags_value ON items_tags(value) WHERE plaintext;

            CREATE TABLE items_locks (
                id INTEGER NOT NULL,
                expiry DATETIME NOT NULL,
                PRIMARY KEY(id)
            );

            INSERT INTO profiles (name) VALUES (?1);
            INSERT INTO store_keys (profile_id, value) VALUES (last_insert_rowid(), ?3);
            UPDATE profiles SET active_key_id = last_insert_rowid();

            COMMIT;
        "#,
        )
        .persistent(false)
        .bind(&spec.profile_id)
        .bind(spec.wrap_key_ref)
        .bind(spec.enc_store_key)
        .execute(&mut conn)
        .await?;

        let row = sqlx::query(
            r#"SELECT id, active_key_id FROM profiles WHERE name = ?1
        "#,
        )
        .persistent(false)
        .bind(spec.profile_id)
        .fetch_one(&mut conn)
        .await?;
        let default_profile = row.try_get(0)?;
        let key_id: i64 = row.try_get(1)?;
        let mut key_cache = KeyCache::new(spec.wrap_key);
        key_cache.set_profile_key(default_profile, key_id, spec.store_key);

        Ok(KvSqlite::new(conn_pool, default_profile, key_cache))
    }
}

pub struct KvSqlite {
    conn_pool: SqlitePool,
    default_profile: ProfileId,
    key_cache: KeyCache,
    scans: Mutex<BTreeMap<ScanToken, Scan>>,
    locks: Mutex<BTreeMap<LockToken, Lock>>,
}

impl KvSqlite {
    pub(crate) fn new(
        conn_pool: SqlitePool,
        default_profile: ProfileId,
        key_cache: KeyCache,
    ) -> Self {
        Self {
            conn_pool,
            default_profile,
            key_cache,
            scans: Mutex::new(BTreeMap::new()),
            locks: Mutex::new(BTreeMap::new()),
        }
    }

    async fn get_profile_key(
        &self,
        pid: Option<ProfileId>,
    ) -> KvResult<(KeyId, Option<Arc<StoreKey>>)> {
        if let Some((kid, key)) = self
            .key_cache
            .get_profile_key(pid.unwrap_or(self.default_profile))
        {
            Ok((kid, key))
        } else {
            // FIXME fetch from database
            unimplemented!()
        }
    }
}

impl QueryPrepare for KvSqlite {
    type DB = Sqlite;
}

#[async_trait]
impl KvStore for KvSqlite {
    async fn count(
        &self,
        profile_id: Option<ProfileId>,
        category: &str,
        tag_filter: Option<wql::Query>,
    ) -> KvResult<i64> {
        let (key_id, key) = self.get_profile_key(profile_id).await?;
        let category = match key {
            Some(key) => key.encrypt_entry_category(category)?,
            None => category.as_bytes().to_vec(),
        };
        let mut args = QueryParams::new();
        args.push(key_id);
        args.push(category);
        let query = extend_query::<Self>(COUNT_QUERY, &mut args, tag_filter, None, None)?;
        let count = sqlx::query_scalar_with(query.as_str(), args)
            .fetch_one(&self.conn_pool)
            .await?;
        KvResult::Ok(count)
    }

    async fn fetch(
        &self,
        profile_id: Option<ProfileId>,
        category: &str,
        name: &str,
        options: KvFetchOptions,
    ) -> KvResult<Option<KvEntry>> {
        let (key_id, key) = self.get_profile_key(profile_id).await?;
        let raw_category = category.to_owned();
        let raw_name = name.to_owned();
        let (category, name) = match key.clone() {
            Some(key) => (
                key.encrypt_entry_category(category)?,
                key.encrypt_entry_name(name)?,
            ),
            None => (category.as_bytes().to_vec(), name.as_bytes().to_vec()),
        };
        if let Some(row) = sqlx::query(FETCH_QUERY)
            .bind(key_id)
            .bind(&category)
            .bind(&name)
            .fetch_optional(&self.conn_pool)
            .await?
        {
            let tags = if options.retrieve_tags {
                fetch_row_tags(&self.conn_pool, row.try_get(0)?, key.clone()).await?
            } else {
                None
            };
            let value: &[u8] = row.try_get(1)?;
            let value = if let Some(key) = key {
                key.decrypt_entry_value(value)?
            } else {
                value.to_vec()
            };
            Ok(Some(KvEntry {
                category: raw_category,
                name: raw_name,
                value,
                tags,
            }))
        } else {
            Ok(None)
        }
    }

    async fn scan_start(
        &self,
        profile_id: Option<ProfileId>,
        category: &str,
        options: KvFetchOptions,
        tag_filter: Option<wql::Query>,
        offset: Option<i64>,
        max_rows: Option<i64>,
    ) -> KvResult<ScanToken> {
        let (key_id, key) = self.get_profile_key(profile_id).await?;
        let pool = self.conn_pool.clone();
        let raw_category = category.to_owned();
        let category = match key.clone() {
            Some(key) => key.encrypt_entry_category(category)?,
            None => category.as_bytes().to_vec(),
        };
        let scan = try_stream! {
            let mut params = QueryParams::new();
            params.push(key_id.clone());
            params.push(category.clone());
            let query = extend_query::<KvSqlite>(SCAN_QUERY, &mut params, tag_filter, offset, max_rows)?;
            let mut batch = Vec::with_capacity(PAGE_SIZE);
            let mut rows = sqlx::query_with(query.as_str(), params).fetch(&pool);
            while let Some(row) = rows.next().await {
                let row = row?;
                let tags = if options.retrieve_tags {
                    // FIXME - fetch tags in batches
                    println!("fetch");
                    fetch_row_tags(&pool, row.try_get(0)?, key.clone()).await?
                } else {
                    None
                };
                let name = row.try_get(1)?;
                let value = row.try_get(2)?;
                let (name, value) =
                        (key.decrypt_entry_name(name)?,
                        key.decrypt_entry_value(value)?)
                    ;
                let entry = KvEntry {
                    category: raw_category.clone(),
                    name,
                    value,
                    tags,
                };
                batch.push(entry);
                if batch.len() == PAGE_SIZE {
                    yield batch.split_off(0);
                }
            }
            if batch.len() > 0 {
                yield batch;
            }
        };
        let token = ScanToken::next();
        self.scans.lock().await.insert(token, scan.boxed());
        Ok(token)
    }

    async fn scan_next(
        &self,
        scan_token: ScanToken,
    ) -> KvResult<(Vec<KvEntry>, Option<ScanToken>)> {
        let scan = self.scans.lock().await.remove(&scan_token);
        if let Some(mut scan) = scan {
            match scan.next().await {
                Some(Ok(rows)) => {
                    let token = if rows.len() == PAGE_SIZE {
                        self.scans.lock().await.insert(scan_token, scan);
                        Some(scan_token)
                    } else {
                        None
                    };
                    Ok((rows, token))
                }
                Some(Err(err)) => Err(err),
                None => Ok((vec![], None)),
            }
        } else {
            Err(err_msg!(Timeout))
        }
    }

    async fn update(
        &self,
        entries: Vec<KvUpdateEntry>,
        with_lock: Option<LockToken>,
    ) -> KvResult<()> {
        let mut updates = vec![];
        for update in entries {
            let (key_id, key) = self.get_profile_key(update.profile_id).await?;
            let (enc_entry, enc_tags) = key.encrypt_entry(&update.entry)?;
            updates.push((key_id, enc_entry, enc_tags, update.expire_ms))
        }

        let mut txn = self.conn_pool.begin().await?; // deferred write txn
        for (key_id, enc_entry, enc_tags, expire_ms) in updates {
            let row_id: Option<i64> = sqlx::query_scalar(FETCH_QUERY)
                .bind(&key_id)
                .bind(enc_entry.category.as_ref())
                .bind(enc_entry.name.as_ref())
                .fetch_optional(&mut txn)
                .await?;
            let row_id = if let Some(row_id) = row_id {
                sqlx::query("UPDATE items SET value=?1 WHERE id=?2")
                    .bind(row_id)
                    .bind(enc_entry.value.as_ref())
                    .execute(&mut txn)
                    .await?;
                sqlx::query("DELETE FROM items_tags WHERE item_id=?1")
                    .bind(row_id)
                    .execute(&mut txn)
                    .await?;
                row_id
            } else {
                sqlx::query(INSERT_QUERY)
                    .bind(&key_id)
                    .bind(enc_entry.category.as_ref())
                    .bind(enc_entry.name.as_ref())
                    .bind(enc_entry.value.as_ref())
                    .bind(&expire_ms.map(expiry_timestamp))
                    .execute(&mut txn)
                    .await?
                    .last_insert_rowid()
            };
            if let Some(tags) = enc_tags {
                for tag in tags {
                    sqlx::query(
                        "INSERT INTO items_tags(item_id, name, value, plaintext)
                             VALUES(?1, ?2, ?3, ?4)",
                    )
                    .bind(row_id)
                    .bind(&tag.name)
                    .bind(&tag.value)
                    .bind(tag.plaintext as i32)
                    .execute(&mut txn)
                    .await?;
                }
            }
        }
        Ok(txn.commit().await?)
    }

    async fn create_lock(
        &self,
        lock_info: KvUpdateEntry,
        options: KvFetchOptions,
        acquire_timeout_ms: Option<i64>,
    ) -> KvResult<Option<(LockToken, KvEntry)>> {
        let (key_id, key) = self.get_profile_key(lock_info.profile_id).await?;
        let raw_entry = lock_info.entry.clone();
        let (enc_entry, enc_tags) = key.encrypt_entry(&raw_entry)?;
        let hash = hash_lock_info(key_id, &lock_info);

        let mut txn = self.conn_pool.begin().await?;

        let interval = 10;
        let expire = acquire_timeout_ms.map(|offs| {
            Instant::now() + Duration::from_millis(std::cmp::max(0, offs - interval) as u64)
        });
        loop {
            let upserted = sqlx::query(
                "INSERT INTO items_locks (id, expiry) VALUES (?1, ?2)
                ON CONFLICT (id) DO UPDATE SET expiry=excluded.expiry
                WHERE expiry <= datetime('now')",
            )
            .bind(hash)
            .bind(expiry_timestamp(LOCK_EXPIRY))
            .execute(&mut txn)
            .await?
            .rows_affected();
            if upserted > 0 {
                break;
            }
            if expire
                .map(|exp| Instant::now().checked_duration_since(exp).is_some())
                .unwrap_or(false)
            {
                return Ok(None);
            }
            smol::Timer::after(Duration::from_millis(interval as u64)).await;
        }

        let entry = match sqlx::query(FETCH_QUERY)
            .bind(&key_id)
            .bind(enc_entry.category.as_ref())
            .bind(enc_entry.name.as_ref())
            .fetch_optional(&mut txn)
            .await?
        {
            Some(row) => {
                let value = key.decrypt_entry_value(row.try_get(1)?)?;
                KvEntry {
                    category: raw_entry.category.clone(),
                    name: raw_entry.name.clone(),
                    value,
                    tags: None, // FIXME optionally fetch tags
                }
            }
            None => {
                sqlx::query(INSERT_QUERY)
                    .bind(&key_id)
                    .bind(enc_entry.category.as_ref())
                    .bind(enc_entry.name.as_ref())
                    .bind(enc_entry.value.as_ref())
                    .bind(&lock_info.expire_ms.map(expiry_timestamp))
                    .execute(&mut txn)
                    .await?;
                raw_entry
            }
        };
        txn.commit().await?;

        let token = LockToken::next();
        self.locks.lock().await.insert(token, Lock { id: hash });
        Ok(Some((token, entry)))
    }

    async fn close(&self) -> KvResult<()> {
        self.conn_pool.close().await;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db_utils::replace_arg_placeholders;

    #[test]
    fn sqlite_check_expiry_timestamp() {
        suspend::block_on(async {
            let spec = KvProvisionSpec::create_default().await?;
            let db = KvSqliteOptions::in_memory().provision_store(spec).await?;
            let ts = expiry_timestamp(LOCK_EXPIRY);
            let check = sqlx::query("SELECT datetime('now'), ?1, ?1 > datetime('now')")
                .bind(ts)
                .fetch_one(&db.conn_pool)
                .await?;
            let now: String = check.try_get(0)?;
            let cmp_ts: String = check.try_get(1)?;
            let cmp: bool = check.try_get(2)?;
            if !cmp {
                panic!("now ({}) > expiry timestamp ({})", now, cmp_ts);
            }
            KvResult::Ok(())
        })
        .unwrap();
    }

    #[test]
    fn sqlite_simple_and_convert_args_works() {
        assert_eq!(
            replace_arg_placeholders::<KvSqlite>("This $$ is $$ a $$ string!", 3),
            ("This ?3 is ?4 a ?5 string!".to_string(), 6),
        );
        assert_eq!(
            replace_arg_placeholders::<KvSqlite>("This is a string!", 1),
            ("This is a string!".to_string(), 1),
        );
    }
}
