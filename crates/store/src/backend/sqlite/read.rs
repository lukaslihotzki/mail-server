/*
 * Copyright (c) 2023 Stalwart Labs Ltd.
 *
 * This file is part of the Stalwart Mail Server.
 *
 * This program is free software: you can redistribute it and/or modify
 * it under the terms of the GNU Affero General Public License as
 * published by the Free Software Foundation, either version 3 of
 * the License, or (at your option) any later version.
 *
 * This program is distributed in the hope that it will be useful,
 * but WITHOUT ANY WARRANTY; without even the implied warranty of
 * MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE. See the
 * GNU Affero General Public License for more details.
 * in the LICENSE file at the top-level directory of this distribution.
 * You should have received a copy of the GNU Affero General Public License
 * along with this program.  If not, see <http://www.gnu.org/licenses/>.
 *
 * You can be released from the requirements of the AGPLv3 license by
 * purchasing a commercial license. Please contact licensing@stalw.art
 * for more details.
*/

use roaring::RoaringBitmap;
use rusqlite::OptionalExtension;

use crate::{
    query::{self, Operator},
    write::{
        key::{DeserializeBigEndian, KeySerializer},
        BitmapClass, ValueClass,
    },
    BitmapKey, Deserialize, IndexKey, IndexKeyPrefix, Key, LogKey, StoreRead, ValueKey,
};

use super::{SqliteStore, BITS_PER_BLOCK, WORDS_PER_BLOCK, WORD_SIZE_BITS};

#[async_trait::async_trait]
impl StoreRead for SqliteStore {
    async fn get_value<U>(&self, key: impl Key) -> crate::Result<Option<U>>
    where
        U: Deserialize + 'static,
    {
        let conn = self.conn_pool.get()?;
        self.spawn_worker(move || {
            let key = key.serialize(false);
            let mut result = conn.prepare_cached("SELECT v FROM v WHERE k = ?")?;
            result
                .query_row([&key], |row| {
                    U::deserialize(row.get_ref(0)?.as_bytes()?)
                        .map_err(|err| rusqlite::Error::ToSqlConversionFailure(err.into()))
                })
                .optional()
                .map_err(Into::into)
        })
        .await
    }

    async fn get_bitmap(
        &self,
        mut key: BitmapKey<BitmapClass>,
    ) -> crate::Result<Option<RoaringBitmap>> {
        let begin = key.serialize(false);
        key.block_num = u32::MAX;
        let key_len = begin.len();
        let end = key.serialize(false);
        let conn = self.conn_pool.get()?;

        self.spawn_worker(move || {
            let mut bm = RoaringBitmap::new();
            let mut query = conn
                    .prepare_cached("SELECT z, a, b, c, d, e, f, g, h, i, j, k, l, m, n, o, p FROM b WHERE z >= ? AND z <= ?")?;
            let mut rows = query.query([&begin, &end])?;

            while let Some(row) = rows.next()? {
                let key = row.get_ref(0)?.as_bytes()?;
                if key.len() == key_len {
                    let block_num = key.deserialize_be_u32(key.len() - std::mem::size_of::<u32>())?;

                    for word_num in 0..WORDS_PER_BLOCK {
                        match row.get::<_, i64>((word_num + 1) as usize)? as u64 {
                            0 => (),
                            u64::MAX => {
                                bm.insert_range(
                                    block_num * BITS_PER_BLOCK + word_num * WORD_SIZE_BITS
                                        ..(block_num * BITS_PER_BLOCK + word_num * WORD_SIZE_BITS)
                                            + WORD_SIZE_BITS,
                                );
                            }
                            mut word => {
                                while word != 0 {
                                    let trailing_zeros = word.trailing_zeros();
                                    bm.insert(
                                        block_num * BITS_PER_BLOCK
                                            + word_num * WORD_SIZE_BITS
                                            + trailing_zeros,
                                    );
                                    word ^= 1 << trailing_zeros;
                                }
                            }
                        }
                    }
                }
            }
            Ok(if !bm.is_empty() { Some(bm) } else { None })
        }).await
    }

    async fn range_to_bitmap(
        &self,
        account_id: u32,
        collection: u8,
        field: u8,
        value: Vec<u8>,
        op: query::Operator,
    ) -> crate::Result<Option<RoaringBitmap>> {
        let conn = self.conn_pool.get()?;
        self.spawn_worker(move || {
            let k1 = KeySerializer::new(
                std::mem::size_of::<IndexKey<&[u8]>>()
                    + value.len()
                    + 1
                    + std::mem::size_of::<u32>(),
            )
            .write(account_id)
            .write(collection)
            .write(field);
            let k2 = KeySerializer::new(
                std::mem::size_of::<IndexKey<&[u8]>>()
                    + value.len()
                    + 1
                    + std::mem::size_of::<u32>(),
            )
            .write(account_id)
            .write(collection)
            .write(field + matches!(op, Operator::GreaterThan | Operator::GreaterEqualThan) as u8);

            let (query, begin, end) = match op {
                Operator::LowerThan => (
                    ("SELECT k FROM i WHERE k >= ? AND k < ?"),
                    (k1.finalize()),
                    (k2.write(&value[..]).write(0u32).finalize()),
                ),
                Operator::LowerEqualThan => (
                    ("SELECT k FROM i WHERE k >= ? AND k <= ?"),
                    (k1.finalize()),
                    (k2.write(&value[..]).write(u32::MAX).finalize()),
                ),
                Operator::GreaterThan => (
                    ("SELECT k FROM i WHERE k > ? AND k <= ?"),
                    (k1.write(&value[..]).write(u32::MAX).finalize()),
                    (k2.finalize()),
                ),
                Operator::GreaterEqualThan => (
                    ("SELECT k FROM i WHERE k >= ? AND k <= ?"),
                    (k1.write(&value[..]).write(0u32).finalize()),
                    (k2.finalize()),
                ),
                Operator::Equal => (
                    ("SELECT k FROM i WHERE k >= ? AND k <= ?"),
                    (k1.write(&value[..]).write(0u32).finalize()),
                    (k2.write(&value[..]).write(u32::MAX).finalize()),
                ),
            };

            let mut bm = RoaringBitmap::new();
            let mut query = conn.prepare_cached(query)?;
            let mut rows = query.query([&begin, &end])?;

            if op != Operator::Equal {
                while let Some(row) = rows.next()? {
                    let key = row.get_ref(0)?.as_bytes()?;
                    bm.insert(key.deserialize_be_u32(key.len() - std::mem::size_of::<u32>())?);
                }
            } else {
                let key_len = begin.len();
                while let Some(row) = rows.next()? {
                    let key = row.get_ref(0)?.as_bytes()?;
                    if key.len() == key_len {
                        bm.insert(key.deserialize_be_u32(key.len() - std::mem::size_of::<u32>())?);
                    }
                }
            }

            Ok(Some(bm))
        })
        .await
    }

    async fn sort_index(
        &self,
        account_id: u32,
        collection: impl Into<u8> + Sync + Send,
        field: impl Into<u8> + Sync + Send,
        ascending: bool,
        mut cb: impl for<'x> FnMut(&'x [u8], u32) -> crate::Result<bool> + Sync + Send,
    ) -> crate::Result<()> {
        let collection = collection.into();
        let field = field.into();

        let conn = self.conn_pool.get()?;

        self.spawn_worker(move || {
            let begin = IndexKeyPrefix {
                account_id,
                collection,
                field,
            }
            .serialize(false);
            let end = IndexKeyPrefix {
                account_id,
                collection,
                field: field + 1,
            }
            .serialize(false);
            let prefix_len = begin.len();
            let mut query = conn.prepare_cached(if ascending {
                "SELECT k FROM i WHERE k >= ? AND k < ? ORDER BY k ASC"
            } else {
                "SELECT k FROM i WHERE k >= ? AND k < ? ORDER BY k DESC"
            })?;
            let mut rows = query.query([&begin, &end])?;

            while let Some(row) = rows.next()? {
                let key = row.get_ref(0)?.as_bytes()?;
                let id_pos = key.len() - std::mem::size_of::<u32>();
                debug_assert!(key.starts_with(&begin));
                if !cb(
                    key.get(prefix_len..id_pos).ok_or_else(|| {
                        crate::Error::InternalError("Invalid key found in index".to_string())
                    })?,
                    key.deserialize_be_u32(id_pos)?,
                )? {
                    return Ok(());
                }
            }

            Ok(())
        })
        .await
    }

    async fn iterate(
        &self,
        begin: impl Key,
        end: impl Key,
        first: bool,
        ascending: bool,
        mut cb: impl for<'x> FnMut(&'x [u8], &'x [u8]) -> crate::Result<bool> + Sync + Send,
    ) -> crate::Result<()> {
        let conn = self.conn_pool.get()?;

        self.spawn_worker(move || {
            let table = char::from(begin.subspace());
            let begin = begin.serialize(false);
            let end = end.serialize(false);

            let mut query = conn.prepare_cached(&match (first, ascending) {
                (true, true) => {
                    format!(
                        "SELECT k, v FROM {table} WHERE k >= ? AND k <= ? ORDER BY k ASC LIMIT 1"
                    )
                }
                (true, false) => {
                    format!(
                        "SELECT k, v FROM {table} WHERE k >= ? AND k <= ? ORDER BY k DESC LIMIT 1"
                    )
                }
                (false, true) => {
                    format!("SELECT k, v FROM {table} WHERE k >= ? AND k <= ? ORDER BY k ASC")
                }
                (false, false) => {
                    format!("SELECT k, v FROM {table} WHERE k >= ? AND k <= ? ORDER BY k DESC")
                }
            })?;
            let mut rows = query.query([&begin, &end])?;

            while let Some(row) = rows.next()? {
                let key = row.get_ref(0)?.as_bytes()?;
                let value = row.get_ref(1)?.as_bytes()?;

                if !cb(key, value)? {
                    break;
                }
            }

            Ok(())
        })
        .await
    }

    async fn get_last_change_id(
        &self,
        account_id: u32,
        collection: impl Into<u8> + Sync + Send,
    ) -> crate::Result<Option<u64>> {
        let conn = self.conn_pool.get()?;
        let collection = collection.into();

        self.spawn_worker(move || {
            let key = LogKey {
                account_id,
                collection,
                change_id: u64::MAX,
            }
            .serialize(false);

            conn.prepare_cached("SELECT k FROM l WHERE k < ? ORDER BY k DESC LIMIT 1")?
                .query_row([&key], |row| {
                    let key = row.get_ref(0)?.as_bytes()?;

                    key.deserialize_be_u64(key.len() - std::mem::size_of::<u64>())
                        .map_err(|err| rusqlite::Error::ToSqlConversionFailure(err.into()))
                })
                .optional()
                .map_err(Into::into)
        })
        .await
    }

    async fn get_counter(
        &self,
        key: impl Into<ValueKey<ValueClass>> + Sync + Send,
    ) -> crate::Result<i64> {
        let key = key.into().serialize(false);
        let conn = self.conn_pool.get()?;
        self.spawn_worker(move || {
            match conn
                .prepare_cached("SELECT v FROM q WHERE k = ?")?
                .query_row([&key], |row| row.get::<_, i64>(0))
            {
                Ok(value) => Ok(value),
                Err(rusqlite::Error::QueryReturnedNoRows) => Ok(0),
                Err(e) => Err(e.into()),
            }
        })
        .await
    }

    #[cfg(feature = "test_mode")]
    async fn assert_is_empty(&self) {
        use crate::StorePurge;

        let conn = self.conn_pool.get().unwrap();
        self.purge_bitmaps().await.unwrap();

        self.spawn_worker(move || {
        // Values
        let mut query = conn.prepare_cached("SELECT k, v FROM v").unwrap();
        let mut rows = query.query([]).unwrap();
        let mut has_errors = false;

        while let Some(row) = rows.next().unwrap() {
            let key = row.get_ref(0).unwrap().as_bytes().unwrap();
            let value = row.get_ref(1).unwrap().as_bytes().unwrap();

            if key[0..4] != u32::MAX.to_be_bytes() {
                eprintln!("Table values is not empty: {key:?} {value:?}");
                has_errors = true;
            }
        }

        // Indexes
        let mut query = conn.prepare_cached("SELECT k FROM i").unwrap();
        let mut rows = query.query([]).unwrap();

        while let Some(row) = rows.next().unwrap() {
            let key = row.get_ref(0).unwrap().as_bytes().unwrap();

            eprintln!(
                    "Table index is not empty, account {}, collection {}, document {}, property {}, value {:?}: {:?}",
                    u32::from_be_bytes(key[0..4].try_into().unwrap()),
                    key[4],
                    u32::from_be_bytes(key[key.len()-4..].try_into().unwrap()),
                    key[5],
                    String::from_utf8_lossy(&key[6..key.len()-4]),
                    key
                );
            has_errors = true;
        }

        // Bitmaps
        let mut query = conn
            .prepare_cached("SELECT z, a, b, c, d, e, f, g, h, i, j, k, l, m, n, o, p FROM b")
            .unwrap();
        let mut rows = query.query([]).unwrap();

        'outer: while let Some(row) = rows.next().unwrap() {
            let key = row.get_ref(0).unwrap().as_bytes().unwrap();
            if key[0..4] != u32::MAX.to_be_bytes() {
                for bit_pos in 1..=16 {
                    let bit_value = row.get::<_, i64>(bit_pos).unwrap() as u64;
                    if bit_value != 0 {
                        eprintln!("Table bitmaps is not empty: {key:?} {bit_pos} {bit_value}");
                        has_errors = true;

                        continue 'outer;
                    }
                }
                eprintln!("Table bitmaps failed to purge, found key: {key:?}");
                has_errors = true;
            }
        }

        // Quotas
        let mut query = conn.prepare_cached("SELECT k, v FROM q").unwrap();
        let mut rows = query.query([]).unwrap();

        while let Some(row) = rows.next().unwrap() {
            let key = row.get_ref(0).unwrap().as_bytes().unwrap();
            let value = row.get::<_, i64>(1).unwrap();
            if value != 0 {
                eprintln!(
                    "Table counter is not empty, account {:?}, quota: {}",
                    key, value,
                );
                has_errors = true;
            }
        }

        // Delete logs
        conn.execute("DELETE FROM l", []).unwrap();

        if has_errors {
            panic!("Database is not empty");
        }

        Ok(())
        }).await.unwrap();
        self.id_assigner.lock().clear();
    }
}
