pub use crate::model::now_ms;
use crate::{
    auth,
    config::{Config, private_dir, private_write},
    store::Store,
};
use anyhow::Result;
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Value, json};
use std::{
    path::PathBuf,
    sync::{Arc, Mutex, RwLock},
};
use tokio::sync::{Semaphore, watch};

pub struct AppState {
    pub connection_lock: Mutex<()>,
    pub home: PathBuf,
    pub config: RwLock<Config>,
    pub db: Mutex<Connection>,
    pub changes: watch::Sender<u64>,
    pub started_at: i64,
    pub addresses: RwLock<Value>,
    pub login_gate: Arc<Semaphore>,
    pub check_gate: Arc<Semaphore>,
    pub setup_gate: Semaphore,
    pub runtime_failure: RwLock<Option<Value>>,
}
impl AppState {
    pub fn open(home: PathBuf) -> Result<Self> {
        let config = Config::load(&home)?;
        config.validate()?;
        private_dir(&home.join("secrets"))?;
        let db = Store::open(&home)?.db;
        db.execute_batch("CREATE TABLE IF NOT EXISTS records(collection TEXT NOT NULL,id TEXT NOT NULL,value TEXT NOT NULL,updated_at INTEGER NOT NULL,PRIMARY KEY(collection,id)); CREATE TABLE IF NOT EXISTS tokens(hash TEXT PRIMARY KEY,id TEXT NOT NULL,name TEXT NOT NULL,scopes TEXT NOT NULL,expires_at INTEGER NOT NULL,created_at INTEGER NOT NULL); CREATE TABLE IF NOT EXISTS browser_sessions(hash TEXT PRIMARY KEY,owner TEXT NOT NULL,csrf TEXT NOT NULL,expires_at INTEGER NOT NULL); CREATE TABLE IF NOT EXISTS login_attempts(key TEXT PRIMARY KEY,failures INTEGER NOT NULL,locked_until INTEGER NOT NULL);")?;
        let path = home.join("secrets/control-token");
        let control = if path.exists() {
            std::fs::read_to_string(&path)?.trim().to_owned()
        } else {
            let value = auth::random_token();
            private_write(&path, value.as_bytes())?;
            value
        };
        anyhow::ensure!(control.len() >= 32, "Local control credential is invalid");
        db.execute(
            "DELETE FROM tokens WHERE id='local-control' AND hash<>?1",
            [auth::digest(&control)],
        )?;
        db.execute("INSERT OR IGNORE INTO tokens(hash,id,name,scopes,expires_at,created_at) VALUES(?1,'local-control','Local owner control','[\"admin\"]',?2,?3)",params![auth::digest(&control),now_ms()+100_i64*366*86400*1000,now_ms()])?;
        let revision = config.revision;
        Ok(Self {
            connection_lock: Mutex::new(()),
            home,
            config: RwLock::new(config),
            db: Mutex::new(db),
            changes: watch::channel(revision).0,
            started_at: now_ms(),
            addresses: RwLock::new(json!({})),
            login_gate: Arc::new(Semaphore::new(2)),
            check_gate: Arc::new(Semaphore::new(1)),
            setup_gate: Semaphore::new(1),
            runtime_failure: RwLock::new(None),
        })
    }
    pub fn instance_config(&self) -> Config {
        self.config.read().unwrap().clone()
    }
    pub fn event(&self, kind: &str, data: Value) -> Result<()> {
        self.db.lock().unwrap().execute(
            "INSERT INTO audit(time,kind,data) VALUES(?1,?2,?3)",
            params![now_ms(), kind, data.to_string()],
        )?;
        self.db.lock().unwrap().execute(
            "DELETE FROM audit WHERE seq <= (SELECT COALESCE(MAX(seq),0)-5000 FROM audit)",
            [],
        )?;
        Ok(())
    }
    pub fn set_record(&self, collection: &str, id: &str, value: Value) -> Result<()> {
        self.db.lock().unwrap().execute("INSERT INTO records(collection,id,value,updated_at) VALUES(?1,?2,?3,?4) ON CONFLICT(collection,id) DO UPDATE SET value=excluded.value,updated_at=excluded.updated_at",params![collection,id,value.to_string(),now_ms()])?;
        Ok(())
    }
    pub fn get_record(&self, collection: &str, id: &str) -> Result<Option<Value>> {
        let text: Option<String> = self
            .db
            .lock()
            .unwrap()
            .query_row(
                "SELECT value FROM records WHERE collection=?1 AND id=?2",
                params![collection, id],
                |r| r.get(0),
            )
            .optional()?;
        text.map(|s| Ok(serde_json::from_str(&s)?)).transpose()
    }
    pub fn list_records(&self, collection: &str) -> Result<Vec<Value>> {
        let db = self.db.lock().unwrap();
        let mut stmt = db.prepare(
            "SELECT value FROM records WHERE collection=?1 ORDER BY updated_at DESC LIMIT 1000",
        )?;
        let rows = stmt
            .query_map([collection], |r| r.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        rows.into_iter()
            .map(|v| Ok(serde_json::from_str(&v)?))
            .collect()
    }
    pub fn observed(&self) -> Result<bool> {
        Ok(self.db.lock().unwrap().query_row("SELECT EXISTS(SELECT 1 FROM contexts) OR EXISTS(SELECT 1 FROM settings WHERE key='identity')",[],|r|r.get(0))?)
    }
}
