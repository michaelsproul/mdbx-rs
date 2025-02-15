use crate::{
    database::Database,
    environment::{Environment, EnvironmentKind, NoWriteMap, TxnManagerMessage, TxnPtr},
    error::{mdbx_result, Result},
    flags::{DatabaseFlags, WriteFlags},
    Cursor, Error, Stat, TableObject,
};
use ffi::{MDBX_txn_flags_t, MDBX_TXN_RDONLY, MDBX_TXN_READWRITE};
use indexmap::IndexSet;
use libc::{c_uint, c_void};
use parking_lot::Mutex;
use std::{
    fmt,
    fmt::Debug,
    marker::PhantomData,
    mem::size_of,
    ptr, result, slice,
    sync::{mpsc::sync_channel, Arc},
};

mod private {
    use super::*;

    pub trait Sealed {}

    impl<'env> Sealed for RO {}
    impl<'env> Sealed for RW {}
}

pub trait TransactionKind: private::Sealed + Debug + 'static {
    #[doc(hidden)]
    const ONLY_CLEAN: bool;

    #[doc(hidden)]
    const OPEN_FLAGS: MDBX_txn_flags_t;
}

#[derive(Debug)]
pub struct RO;
#[derive(Debug)]
pub struct RW;

impl TransactionKind for RO {
    const ONLY_CLEAN: bool = true;
    const OPEN_FLAGS: MDBX_txn_flags_t = MDBX_TXN_RDONLY;
}
impl TransactionKind for RW {
    const ONLY_CLEAN: bool = false;
    const OPEN_FLAGS: MDBX_txn_flags_t = MDBX_TXN_READWRITE;
}

/// An MDBX transaction.
///
/// All database operations require a transaction.
pub struct Transaction<'env, K, E>
where
    K: TransactionKind,
    E: EnvironmentKind,
{
    txn: Arc<Mutex<*mut ffi::MDBX_txn>>,
    primed_dbis: Mutex<IndexSet<ffi::MDBX_dbi>>,
    committed: bool,
    env: &'env Environment<E>,
    _marker: PhantomData<fn(K)>,
}

impl<'env, K, E> Transaction<'env, K, E>
where
    K: TransactionKind,
    E: EnvironmentKind,
{
    pub(crate) fn new(env: &'env Environment<E>) -> Result<Self> {
        let mut txn: *mut ffi::MDBX_txn = ptr::null_mut();
        unsafe {
            mdbx_result(ffi::mdbx_txn_begin_ex(
                env.env(),
                ptr::null_mut(),
                K::OPEN_FLAGS,
                &mut txn,
                ptr::null_mut(),
            ))?;
            Ok(Self::new_from_ptr(env, txn))
        }
    }

    pub(crate) fn new_from_ptr(env: &'env Environment<E>, txn: *mut ffi::MDBX_txn) -> Self {
        Self {
            txn: Arc::new(Mutex::new(txn)),
            primed_dbis: Mutex::new(IndexSet::new()),
            committed: false,
            env,
            _marker: PhantomData,
        }
    }

    /// Returns a raw pointer to the underlying MDBX transaction.
    ///
    /// The caller **must** ensure that the pointer is not used after the
    /// lifetime of the transaction.
    pub(crate) fn txn_mutex(&self) -> Arc<Mutex<*mut ffi::MDBX_txn>> {
        self.txn.clone()
    }

    pub fn txn(&self) -> *mut ffi::MDBX_txn {
        *self.txn.lock()
    }

    /// Returns a raw pointer to the MDBX environment.
    pub fn env(&self) -> &Environment<E> {
        self.env
    }

    /// Returns the transaction id.
    pub fn id(&self) -> u64 {
        txn_execute(&self.txn, |txn| unsafe { ffi::mdbx_txn_id(txn) })
    }

    /// Gets an item from a database.
    ///
    /// This function retrieves the data associated with the given key in the
    /// database. If the database supports duplicate keys
    /// ([DatabaseFlags::DUP_SORT]) then the first data item for the key will be
    /// returned. Retrieval of other items requires the use of
    /// [Cursor]. If the item is not in the database, then
    /// [None] will be returned.
    pub fn get<'txn, Key>(&'txn self, db: &Database<'txn>, key: &[u8]) -> Result<Option<Key>>
    where
        Key: TableObject<'txn>,
    {
        let key_val: ffi::MDBX_val = ffi::MDBX_val {
            iov_len: key.len(),
            iov_base: key.as_ptr() as *mut c_void,
        };
        let mut data_val: ffi::MDBX_val = ffi::MDBX_val {
            iov_len: 0,
            iov_base: ptr::null_mut(),
        };

        txn_execute(&self.txn, |txn| unsafe {
            match ffi::mdbx_get(txn, db.dbi(), &key_val, &mut data_val) {
                ffi::MDBX_SUCCESS => Key::decode_val::<K>(txn, &data_val).map(Some),
                ffi::MDBX_NOTFOUND => Ok(None),
                err_code => Err(Error::from_err_code(err_code)),
            }
        })
    }

    /// Commits the transaction.
    ///
    /// Any pending operations will be saved.
    pub fn commit(self) -> Result<bool> {
        self.commit_and_rebind_open_dbs().map(|v| v.0)
    }

    pub fn prime_for_permaopen(&self, db: Database<'_>) {
        self.primed_dbis.lock().insert(db.dbi());
    }

    /// Commits the transaction and returns table handles permanently open for the lifetime of `Environment`.
    pub fn commit_and_rebind_open_dbs(mut self) -> Result<(bool, Vec<Database<'env>>)> {
        let txnlck = self.txn.lock();
        let txn = *txnlck;
        let result = if K::ONLY_CLEAN {
            mdbx_result(unsafe { ffi::mdbx_txn_commit_ex(txn, ptr::null_mut()) })
        } else {
            let (sender, rx) = sync_channel(0);
            self.env
                .txn_manager
                .as_ref()
                .unwrap()
                .send(TxnManagerMessage::Commit {
                    tx: TxnPtr(txn),
                    sender,
                })
                .unwrap();
            rx.recv().unwrap()
        };
        self.committed = true;
        result.map(|v| {
            (
                v,
                self.primed_dbis
                    .lock()
                    .iter()
                    .map(|&dbi| Database::new_from_ptr(dbi))
                    .collect(),
            )
        })
    }

    /// Opens a handle to an MDBX database.
    ///
    /// If `name` is [None], then the returned handle will be for the default database.
    ///
    /// If `name` is not [None], then the returned handle will be for a named database. In this
    /// case the environment must be configured to allow named databases through
    /// [EnvironmentBuilder::set_max_dbs()](crate::EnvironmentBuilder::set_max_dbs).
    ///
    /// The returned database handle may be shared among any transaction in the environment.
    ///
    /// The database name may not contain the null character.
    pub fn open_db<'txn>(&'txn self, name: Option<&str>) -> Result<Database<'txn>> {
        Database::new(self, name, 0)
    }

    /// Gets the option flags for the given database in the transaction.
    pub fn db_flags<'txn>(&'txn self, db: &Database<'txn>) -> Result<DatabaseFlags> {
        let mut flags: c_uint = 0;
        unsafe {
            mdbx_result(txn_execute(&self.txn, |txn| {
                ffi::mdbx_dbi_flags_ex(txn, db.dbi(), &mut flags, ptr::null_mut())
            }))?;
        }
        Ok(DatabaseFlags::from_bits_truncate(flags))
    }

    /// Retrieves database statistics.
    pub fn db_stat<'txn>(&'txn self, db: &Database<'txn>) -> Result<Stat> {
        unsafe {
            let mut stat = Stat::new();
            mdbx_result(txn_execute(&self.txn, |txn| {
                ffi::mdbx_dbi_stat(txn, db.dbi(), stat.mdb_stat(), size_of::<Stat>())
            }))?;
            Ok(stat)
        }
    }

    /// Open a new cursor on the given database.
    pub fn cursor<'txn>(&'txn self, db: &Database<'txn>) -> Result<Cursor<'txn, K>> {
        Cursor::new(self, db)
    }
}

pub(crate) fn txn_execute<F: FnOnce(*mut ffi::MDBX_txn) -> T, T>(
    txn: &Mutex<*mut ffi::MDBX_txn>,
    f: F,
) -> T {
    let lck = txn.lock();
    (f)(*lck)
}

impl<'env, E> Transaction<'env, RW, E>
where
    E: EnvironmentKind,
{
    fn open_db_with_flags<'txn>(
        &'txn self,
        name: Option<&str>,
        flags: DatabaseFlags,
    ) -> Result<Database<'txn>> {
        Database::new(self, name, flags.bits())
    }

    /// Opens a handle to an MDBX database, creating the database if necessary.
    ///
    /// If the database is already created, the given option flags will be added to it.
    ///
    /// If `name` is [None], then the returned handle will be for the default database.
    ///
    /// If `name` is not [None], then the returned handle will be for a named database. In this
    /// case the environment must be configured to allow named databases through
    /// [EnvironmentBuilder::set_max_dbs()](crate::EnvironmentBuilder::set_max_dbs).
    ///
    /// This function will fail with [Error::BadRslot](crate::error::Error::BadRslot) if called by a thread with an open
    /// transaction.
    pub fn create_db<'txn>(
        &'txn self,
        name: Option<&str>,
        flags: DatabaseFlags,
    ) -> Result<Database<'txn>> {
        self.open_db_with_flags(name, flags | DatabaseFlags::CREATE)
    }

    /// Stores an item into a database.
    ///
    /// This function stores key/data pairs in the database. The default
    /// behavior is to enter the new key/data pair, replacing any previously
    /// existing key if duplicates are disallowed, or adding a duplicate data
    /// item if duplicates are allowed ([DatabaseFlags::DUP_SORT]).
    pub fn put<'txn>(
        &'txn self,
        db: &Database<'txn>,
        key: impl AsRef<[u8]>,
        data: impl AsRef<[u8]>,
        flags: WriteFlags,
    ) -> Result<()> {
        let key = key.as_ref();
        let data = data.as_ref();
        let key_val: ffi::MDBX_val = ffi::MDBX_val {
            iov_len: key.len(),
            iov_base: key.as_ptr() as *mut c_void,
        };
        let mut data_val: ffi::MDBX_val = ffi::MDBX_val {
            iov_len: data.len(),
            iov_base: data.as_ptr() as *mut c_void,
        };
        mdbx_result(txn_execute(&self.txn, |txn| unsafe {
            ffi::mdbx_put(txn, db.dbi(), &key_val, &mut data_val, flags.bits())
        }))?;

        Ok(())
    }

    /// Returns a buffer which can be used to write a value into the item at the
    /// given key and with the given length. The buffer must be completely
    /// filled by the caller.
    pub fn reserve<'txn>(
        &'txn self,
        db: &Database<'txn>,
        key: impl AsRef<[u8]>,
        len: usize,
        flags: WriteFlags,
    ) -> Result<&'txn mut [u8]> {
        let key = key.as_ref();
        let key_val: ffi::MDBX_val = ffi::MDBX_val {
            iov_len: key.len(),
            iov_base: key.as_ptr() as *mut c_void,
        };
        let mut data_val: ffi::MDBX_val = ffi::MDBX_val {
            iov_len: len,
            iov_base: ptr::null_mut::<c_void>(),
        };
        unsafe {
            mdbx_result(txn_execute(&self.txn, |txn| {
                ffi::mdbx_put(
                    txn,
                    db.dbi(),
                    &key_val,
                    &mut data_val,
                    flags.bits() | ffi::MDBX_RESERVE,
                )
            }))?;
            Ok(slice::from_raw_parts_mut(
                data_val.iov_base as *mut u8,
                data_val.iov_len,
            ))
        }
    }

    /// Delete items from a database.
    /// This function removes key/data pairs from the database.
    ///
    /// The data parameter is NOT ignored regardless the database does support sorted duplicate data items or not.
    /// If the data parameter is [Some] only the matching data item will be deleted.
    /// Otherwise, if data parameter is [None], any/all value(s) for specified key will be deleted.
    ///
    /// Returns `true` if the key/value pair was present.
    pub fn del<'txn>(
        &'txn self,
        db: &Database<'txn>,
        key: impl AsRef<[u8]>,
        data: Option<&[u8]>,
    ) -> Result<bool> {
        let key = key.as_ref();
        let key_val: ffi::MDBX_val = ffi::MDBX_val {
            iov_len: key.len(),
            iov_base: key.as_ptr() as *mut c_void,
        };
        let data_val: Option<ffi::MDBX_val> = data.map(|data| ffi::MDBX_val {
            iov_len: data.len(),
            iov_base: data.as_ptr() as *mut c_void,
        });

        mdbx_result({
            txn_execute(&self.txn, |txn| {
                if let Some(d) = data_val {
                    unsafe { ffi::mdbx_del(txn, db.dbi(), &key_val, &d) }
                } else {
                    unsafe { ffi::mdbx_del(txn, db.dbi(), &key_val, ptr::null()) }
                }
            })
        })
        .map(|_| true)
        .or_else(|e| match e {
            Error::NotFound => Ok(false),
            other => Err(other),
        })
    }

    /// Empties the given database. All items will be removed.
    pub fn clear_db<'txn>(&'txn self, db: &Database<'txn>) -> Result<()> {
        mdbx_result(txn_execute(&self.txn, |txn| unsafe {
            ffi::mdbx_drop(txn, db.dbi(), false)
        }))?;

        Ok(())
    }

    /// Drops the database from the environment.
    ///
    /// # Safety
    /// Caller must close ALL other [Database] and [Cursor] instances pointing to the same dbi BEFORE calling this function.
    pub unsafe fn drop_db<'txn>(&'txn self, db: Database<'txn>) -> Result<()> {
        mdbx_result(txn_execute(&self.txn, |txn| {
            ffi::mdbx_drop(txn, db.dbi(), true)
        }))?;

        Ok(())
    }
}

impl<'env, E> Transaction<'env, RO, E>
where
    E: EnvironmentKind,
{
    /// Closes the database handle.
    ///
    /// # Safety
    /// Caller must close ALL other [Database] and [Cursor] instances pointing to the same dbi BEFORE calling this function.
    pub unsafe fn close_db(&self, db: Database<'_>) -> Result<()> {
        mdbx_result(ffi::mdbx_dbi_close(self.env.env(), db.dbi()))?;

        Ok(())
    }
}

impl<'env> Transaction<'env, RW, NoWriteMap> {
    /// Begins a new nested transaction inside of this transaction.
    pub fn begin_nested_txn(&mut self) -> Result<Transaction<'_, RW, NoWriteMap>> {
        txn_execute(&self.txn, |txn| {
            let (tx, rx) = sync_channel(0);
            self.env
                .txn_manager
                .as_ref()
                .unwrap()
                .send(TxnManagerMessage::Begin {
                    parent: TxnPtr(txn),
                    flags: RW::OPEN_FLAGS,
                    sender: tx,
                })
                .unwrap();

            rx.recv()
                .unwrap()
                .map(|ptr| Transaction::new_from_ptr(self.env, ptr.0))
        })
    }
}

impl<'env, K, E> fmt::Debug for Transaction<'env, K, E>
where
    K: TransactionKind,
    E: EnvironmentKind,
{
    fn fmt(&self, f: &mut fmt::Formatter) -> result::Result<(), fmt::Error> {
        f.debug_struct("RoTransaction").finish()
    }
}

impl<'env, K, E> Drop for Transaction<'env, K, E>
where
    K: TransactionKind,
    E: EnvironmentKind,
{
    fn drop(&mut self) {
        txn_execute(&self.txn, |txn| {
            if !self.committed {
                if K::ONLY_CLEAN {
                    unsafe {
                        ffi::mdbx_txn_abort(txn);
                    }
                } else {
                    let (sender, rx) = sync_channel(0);
                    self.env
                        .txn_manager
                        .as_ref()
                        .unwrap()
                        .send(TxnManagerMessage::Abort {
                            tx: TxnPtr(txn),
                            sender,
                        })
                        .unwrap();
                    rx.recv().unwrap().unwrap();
                }
            }
        })
    }
}

unsafe impl<'env, K, E> Send for Transaction<'env, K, E>
where
    K: TransactionKind,
    E: EnvironmentKind,
{
}

unsafe impl<'env, K, E> Sync for Transaction<'env, K, E>
where
    K: TransactionKind,
    E: EnvironmentKind,
{
}

#[cfg(test)]
mod test {
    use crate::{error::*, flags::*, NoWriteMap};
    use std::{
        borrow::Cow,
        io::Write,
        sync::{Arc, Barrier},
        thread::{self, JoinHandle},
    };
    use tempfile::tempdir;

    type Environment = crate::Environment<NoWriteMap>;

    #[test]
    fn test_put_get_del() {
        let dir = tempdir().unwrap();
        let env = Environment::new().open(dir.path()).unwrap();

        let txn = env.begin_rw_txn().unwrap();
        let db = txn.open_db(None).unwrap();
        txn.put(&db, b"key1", b"val1", WriteFlags::empty()).unwrap();
        txn.put(&db, b"key2", b"val2", WriteFlags::empty()).unwrap();
        txn.put(&db, b"key3", b"val3", WriteFlags::empty()).unwrap();
        txn.commit().unwrap();

        let txn = env.begin_rw_txn().unwrap();
        let db = txn.open_db(None).unwrap();
        assert_eq!(txn.get(&db, b"key1").unwrap(), Some(*b"val1"));
        assert_eq!(txn.get(&db, b"key2").unwrap(), Some(*b"val2"));
        assert_eq!(txn.get(&db, b"key3").unwrap(), Some(*b"val3"));
        assert_eq!(txn.get::<()>(&db, b"key").unwrap(), None);

        txn.del(&db, b"key1", None).unwrap();
        assert_eq!(txn.get::<()>(&db, b"key1").unwrap(), None);
    }

    #[test]
    fn test_put_get_del_multi() {
        let dir = tempdir().unwrap();
        let env = Environment::new().open(dir.path()).unwrap();

        let txn = env.begin_rw_txn().unwrap();
        let db = txn.create_db(None, DatabaseFlags::DUP_SORT).unwrap();
        txn.put(&db, b"key1", b"val1", WriteFlags::empty()).unwrap();
        txn.put(&db, b"key1", b"val2", WriteFlags::empty()).unwrap();
        txn.put(&db, b"key1", b"val3", WriteFlags::empty()).unwrap();
        txn.put(&db, b"key2", b"val1", WriteFlags::empty()).unwrap();
        txn.put(&db, b"key2", b"val2", WriteFlags::empty()).unwrap();
        txn.put(&db, b"key2", b"val3", WriteFlags::empty()).unwrap();
        txn.put(&db, b"key3", b"val1", WriteFlags::empty()).unwrap();
        txn.put(&db, b"key3", b"val2", WriteFlags::empty()).unwrap();
        txn.put(&db, b"key3", b"val3", WriteFlags::empty()).unwrap();
        txn.commit().unwrap();

        let txn = env.begin_rw_txn().unwrap();
        let db = txn.open_db(None).unwrap();
        {
            let mut cur = txn.cursor(&db).unwrap();
            let iter = cur.iter_dup_of::<(), [u8; 4]>(b"key1");
            let vals = iter.map(|x| x.unwrap()).map(|(_, x)| x).collect::<Vec<_>>();
            assert_eq!(vals, vec![*b"val1", *b"val2", *b"val3"]);
        }
        txn.commit().unwrap();

        let txn = env.begin_rw_txn().unwrap();
        let db = txn.open_db(None).unwrap();
        txn.del(&db, b"key1", Some(b"val2")).unwrap();
        txn.del(&db, b"key2", None).unwrap();
        txn.commit().unwrap();

        let txn = env.begin_rw_txn().unwrap();
        let db = txn.open_db(None).unwrap();
        {
            let mut cur = txn.cursor(&db).unwrap();
            let iter = cur.iter_dup_of::<(), [u8; 4]>(b"key1");
            let vals = iter.map(|x| x.unwrap()).map(|(_, x)| x).collect::<Vec<_>>();
            assert_eq!(vals, vec![*b"val1", *b"val3"]);

            let iter = cur.iter_dup_of::<(), ()>(b"key2");
            assert_eq!(0, iter.count());
        }
        txn.commit().unwrap();
    }

    #[test]
    fn test_put_get_del_empty_key() {
        let dir = tempdir().unwrap();
        let env = Environment::new().open(dir.path()).unwrap();

        let txn = env.begin_rw_txn().unwrap();
        let db = txn.create_db(None, Default::default()).unwrap();
        txn.put(&db, b"", b"hello", WriteFlags::empty()).unwrap();
        assert_eq!(txn.get(&db, b"").unwrap(), Some(*b"hello"));
        txn.commit().unwrap();

        let txn = env.begin_rw_txn().unwrap();
        let db = txn.open_db(None).unwrap();
        assert_eq!(txn.get(&db, b"").unwrap(), Some(*b"hello"));
        txn.put(&db, b"", b"", WriteFlags::empty()).unwrap();
        assert_eq!(txn.get(&db, b"").unwrap(), Some(*b""));
    }

    #[test]
    fn test_reserve() {
        let dir = tempdir().unwrap();
        let env = Environment::new().open(dir.path()).unwrap();

        let txn = env.begin_rw_txn().unwrap();
        let db = txn.open_db(None).unwrap();
        {
            let mut writer = txn.reserve(&db, b"key1", 4, WriteFlags::empty()).unwrap();
            writer.write_all(b"val1").unwrap();
        }
        txn.commit().unwrap();

        let txn = env.begin_rw_txn().unwrap();
        let db = txn.open_db(None).unwrap();
        assert_eq!(txn.get(&db, b"key1").unwrap(), Some(*b"val1"));
        assert_eq!(txn.get::<()>(&db, b"key").unwrap(), None);

        txn.del(&db, b"key1", None).unwrap();
        assert_eq!(txn.get::<()>(&db, b"key1").unwrap(), None);
    }

    #[test]
    fn test_nested_txn() {
        let dir = tempdir().unwrap();
        let env = Environment::new().open(dir.path()).unwrap();

        let mut txn = env.begin_rw_txn().unwrap();
        txn.put(
            &txn.open_db(None).unwrap(),
            b"key1",
            b"val1",
            WriteFlags::empty(),
        )
        .unwrap();

        {
            let nested = txn.begin_nested_txn().unwrap();
            let db = nested.open_db(None).unwrap();
            nested
                .put(&db, b"key2", b"val2", WriteFlags::empty())
                .unwrap();
            assert_eq!(nested.get(&db, b"key1").unwrap(), Some(*b"val1"));
            assert_eq!(nested.get(&db, b"key2").unwrap(), Some(*b"val2"));
        }

        let db = txn.open_db(None).unwrap();
        assert_eq!(txn.get(&db, b"key1").unwrap(), Some(*b"val1"));
        assert_eq!(txn.get::<()>(&db, b"key2").unwrap(), None);
    }

    #[test]
    fn test_clear_db() {
        let dir = tempdir().unwrap();
        let env = Environment::new().open(dir.path()).unwrap();

        {
            let txn = env.begin_rw_txn().unwrap();
            txn.put(
                &txn.open_db(None).unwrap(),
                b"key",
                b"val",
                WriteFlags::empty(),
            )
            .unwrap();
            assert!(!txn.commit().unwrap());
        }

        {
            let txn = env.begin_rw_txn().unwrap();
            txn.clear_db(&txn.open_db(None).unwrap()).unwrap();
            assert!(!txn.commit().unwrap());
        }

        let txn = env.begin_ro_txn().unwrap();
        assert_eq!(
            txn.get::<()>(&txn.open_db(None).unwrap(), b"key").unwrap(),
            None
        );
    }

    #[test]
    fn test_drop_db() {
        let dir = tempdir().unwrap();
        {
            let env = Environment::new().set_max_dbs(2).open(dir.path()).unwrap();

            {
                let txn = env.begin_rw_txn().unwrap();
                txn.put(
                    &txn.create_db(Some("test"), DatabaseFlags::empty()).unwrap(),
                    b"key",
                    b"val",
                    WriteFlags::empty(),
                )
                .unwrap();
                // Workaround for MDBX dbi drop issue
                txn.create_db(Some("canary"), DatabaseFlags::empty())
                    .unwrap();
                assert!(!txn.commit().unwrap());
            }
            {
                let txn = env.begin_rw_txn().unwrap();
                let db = txn.open_db(Some("test")).unwrap();
                unsafe {
                    txn.drop_db(db).unwrap();
                }
                assert!(matches!(
                    txn.open_db(Some("test")).unwrap_err(),
                    Error::NotFound
                ));
                assert!(!txn.commit().unwrap());
            }
        }

        let env = Environment::new().set_max_dbs(2).open(dir.path()).unwrap();

        let txn = env.begin_ro_txn().unwrap();
        txn.open_db(Some("canary")).unwrap();
        assert!(matches!(
            txn.open_db(Some("test")).unwrap_err(),
            Error::NotFound
        ));
    }

    #[test]
    fn test_concurrent_readers_single_writer() {
        let dir = tempdir().unwrap();
        let env: Arc<Environment> = Arc::new(Environment::new().open(dir.path()).unwrap());

        let n = 10usize; // Number of concurrent readers
        let barrier = Arc::new(Barrier::new(n + 1));
        let mut threads: Vec<JoinHandle<bool>> = Vec::with_capacity(n);

        let key = b"key";
        let val = b"val";

        for _ in 0..n {
            let reader_env = env.clone();
            let reader_barrier = barrier.clone();

            threads.push(thread::spawn(move || {
                {
                    let txn = reader_env.begin_ro_txn().unwrap();
                    let db = txn.open_db(None).unwrap();
                    assert_eq!(txn.get::<()>(&db, key).unwrap(), None);
                }
                reader_barrier.wait();
                reader_barrier.wait();
                {
                    let txn = reader_env.begin_ro_txn().unwrap();
                    let db = txn.open_db(None).unwrap();
                    txn.get::<[u8; 3]>(&db, key).unwrap().unwrap() == *val
                }
            }));
        }

        let txn = env.begin_rw_txn().unwrap();
        let db = txn.open_db(None).unwrap();
        println!("wait2");
        barrier.wait();
        txn.put(&db, key, val, WriteFlags::empty()).unwrap();
        txn.commit().unwrap();

        println!("wait1");
        barrier.wait();

        assert!(threads.into_iter().all(|b| b.join().unwrap()))
    }

    #[test]
    fn test_concurrent_writers() {
        let dir = tempdir().unwrap();
        let env = Arc::new(Environment::new().open(dir.path()).unwrap());

        let n = 10usize; // Number of concurrent writers
        let mut threads: Vec<JoinHandle<bool>> = Vec::with_capacity(n);

        let key = "key";
        let val = "val";

        for i in 0..n {
            let writer_env = env.clone();

            threads.push(thread::spawn(move || {
                let txn = writer_env.begin_rw_txn().unwrap();
                let db = txn.open_db(None).unwrap();
                txn.put(
                    &db,
                    &format!("{}{}", key, i),
                    &format!("{}{}", val, i),
                    WriteFlags::empty(),
                )
                .unwrap();
                txn.commit().is_ok()
            }));
        }
        assert!(threads.into_iter().all(|b| b.join().unwrap()));

        let txn = env.begin_ro_txn().unwrap();
        let db = txn.open_db(None).unwrap();

        for i in 0..n {
            assert_eq!(
                Cow::<Vec<u8>>::Owned(format!("{}{}", val, i).into_bytes()),
                txn.get(&db, format!("{}{}", key, i).as_bytes())
                    .unwrap()
                    .unwrap()
            );
        }
    }

    #[test]
    fn test_stat() {
        let dir = tempdir().unwrap();
        let env = Environment::new().open(dir.path()).unwrap();

        let txn = env.begin_rw_txn().unwrap();
        let db = txn.create_db(None, DatabaseFlags::empty()).unwrap();
        txn.put(&db, b"key1", b"val1", WriteFlags::empty()).unwrap();
        txn.put(&db, b"key2", b"val2", WriteFlags::empty()).unwrap();
        txn.put(&db, b"key3", b"val3", WriteFlags::empty()).unwrap();
        txn.commit().unwrap();

        {
            let txn = env.begin_ro_txn().unwrap();
            let db = txn.open_db(None).unwrap();
            let stat = txn.db_stat(&db).unwrap();
            assert_eq!(stat.entries(), 3);
        }

        let txn = env.begin_rw_txn().unwrap();
        let db = txn.open_db(None).unwrap();
        txn.del(&db, b"key1", None).unwrap();
        txn.del(&db, b"key2", None).unwrap();
        txn.commit().unwrap();

        {
            let txn = env.begin_ro_txn().unwrap();
            let db = txn.open_db(None).unwrap();
            let stat = txn.db_stat(&db).unwrap();
            assert_eq!(stat.entries(), 1);
        }

        let txn = env.begin_rw_txn().unwrap();
        let db = txn.open_db(None).unwrap();
        txn.put(&db, b"key4", b"val4", WriteFlags::empty()).unwrap();
        txn.put(&db, b"key5", b"val5", WriteFlags::empty()).unwrap();
        txn.put(&db, b"key6", b"val6", WriteFlags::empty()).unwrap();
        txn.commit().unwrap();

        {
            let txn = env.begin_ro_txn().unwrap();
            let db = txn.open_db(None).unwrap();
            let stat = txn.db_stat(&db).unwrap();
            assert_eq!(stat.entries(), 4);
        }
    }

    #[test]
    fn test_stat_dupsort() {
        let dir = tempdir().unwrap();
        let env = Environment::new().open(dir.path()).unwrap();

        let txn = env.begin_rw_txn().unwrap();
        let db = txn.create_db(None, DatabaseFlags::DUP_SORT).unwrap();
        txn.put(&db, b"key1", b"val1", WriteFlags::empty()).unwrap();
        txn.put(&db, b"key1", b"val2", WriteFlags::empty()).unwrap();
        txn.put(&db, b"key1", b"val3", WriteFlags::empty()).unwrap();
        txn.put(&db, b"key2", b"val1", WriteFlags::empty()).unwrap();
        txn.put(&db, b"key2", b"val2", WriteFlags::empty()).unwrap();
        txn.put(&db, b"key2", b"val3", WriteFlags::empty()).unwrap();
        txn.put(&db, b"key3", b"val1", WriteFlags::empty()).unwrap();
        txn.put(&db, b"key3", b"val2", WriteFlags::empty()).unwrap();
        txn.put(&db, b"key3", b"val3", WriteFlags::empty()).unwrap();
        txn.commit().unwrap();

        {
            let txn = env.begin_ro_txn().unwrap();
            let stat = txn.db_stat(&txn.open_db(None).unwrap()).unwrap();
            assert_eq!(stat.entries(), 9);
        }

        let txn = env.begin_rw_txn().unwrap();
        let db = txn.open_db(None).unwrap();
        txn.del(&db, b"key1", Some(b"val2")).unwrap();
        txn.del(&db, b"key2", None).unwrap();
        txn.commit().unwrap();

        {
            let txn = env.begin_ro_txn().unwrap();
            let stat = txn.db_stat(&txn.open_db(None).unwrap()).unwrap();
            assert_eq!(stat.entries(), 5);
        }

        let txn = env.begin_rw_txn().unwrap();
        let db = txn.open_db(None).unwrap();
        txn.put(&db, b"key4", b"val1", WriteFlags::empty()).unwrap();
        txn.put(&db, b"key4", b"val2", WriteFlags::empty()).unwrap();
        txn.put(&db, b"key4", b"val3", WriteFlags::empty()).unwrap();
        txn.commit().unwrap();

        {
            let txn = env.begin_ro_txn().unwrap();
            let stat = txn.db_stat(&txn.open_db(None).unwrap()).unwrap();
            assert_eq!(stat.entries(), 8);
        }
    }
}
