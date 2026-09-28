//! Bench's database explorer: the connections you have saved, the catalogs
//! behind them, and the SQL you run against them.
//!
//! What every panel and console shows lives on [`DatabaseStore`], a global, for
//! the reason the Linear crate gives for its own: Bench opens one workspace per
//! worktree, so a panel is per worktree, and a connection is not. Saving a
//! connection in one worktree has to show it in the next.
//!
//! Only Postgres so far. The pieces a second driver would need — connecting,
//! listing a catalog, running a statement — are the three `postgres` functions
//! at the bottom of this file, and nothing above them names sqlx.

pub mod connection_modal;
pub mod database_panel;
pub mod query_view;

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result, anyhow};
use credentials_provider::CredentialsProvider;
use futures::FutureExt as _;
use futures::future::Shared;
use gpui::{
    App, AppContext as _, AsyncApp, Context, Entity, EventEmitter, Global, SharedString, Task,
};
use serde::{Deserialize, Serialize};
use sqlx::postgres::{PgConnectOptions, PgPool, PgPoolOptions, PgSslMode};
use util::ResultExt as _;

pub use connection_modal::ConnectionModal;
pub use database_panel::DatabasePanel;
pub use query_view::QueryView;

/// Where the saved connections live. The password is not among them; it goes
/// to the system keychain, keyed by [`credentials_url`].
const CONNECTIONS_KEY: &str = "database_explorer_connections";

/// How many rows a console keeps from one statement. A `SELECT` without a
/// `LIMIT` against a large table would otherwise pull the table into memory.
pub const MAX_ROWS: usize = 1_000;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);

pub fn init(cx: &mut App) {
    let store = cx.new(DatabaseStore::new);
    cx.set_global(GlobalDatabaseStore(store));
    database_panel::init(cx);
    query_view::init(cx);
}

/// A saved connection's identity, which outlives its name and its host so that
/// renaming one does not orphan its password.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct ConnectionId(pub u64);

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SslMode {
    Disable,
    #[default]
    Prefer,
    Require,
}

impl SslMode {
    pub const ALL: [SslMode; 3] = [SslMode::Disable, SslMode::Prefer, SslMode::Require];

    pub fn label(self) -> &'static str {
        match self {
            SslMode::Disable => "disable",
            SslMode::Prefer => "prefer",
            SslMode::Require => "require",
        }
    }

    pub fn from_label(label: &str) -> Option<Self> {
        SslMode::ALL.into_iter().find(|mode| mode.label() == label)
    }
}

impl From<SslMode> for PgSslMode {
    fn from(mode: SslMode) -> Self {
        match mode {
            SslMode::Disable => PgSslMode::Disable,
            SslMode::Prefer => PgSslMode::Prefer,
            SslMode::Require => PgSslMode::Require,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConnectionConfig {
    pub id: ConnectionId,
    pub name: SharedString,
    pub host: String,
    pub port: u16,
    pub user: String,
    /// The database connected to before one is chosen in the tree; Postgres
    /// has no connection that is not to a database.
    pub database: String,
    #[serde(default)]
    pub ssl_mode: SslMode,
}

impl ConnectionConfig {
    /// What the connection is, without the password, for a tooltip or a line
    /// sent to the agent.
    pub fn url(&self) -> String {
        format!(
            "postgres://{}@{}:{}/{}",
            self.user, self.host, self.port, self.database
        )
    }
}

/// Where a connection's password is kept. Keyed by the connection's id rather
/// than by its host, so two connections to the same server keep their own.
fn credentials_url(id: ConnectionId) -> String {
    format!("bench-database://postgres/{}", id.0)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ObjectKind {
    Connection,
    Database,
    Schema,
    Table,
    Column,
}

/// What a row of the tree points at. Every level is optional below the
/// connection, so one type addresses all five.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ObjectPath {
    pub connection: ConnectionId,
    pub database: Option<SharedString>,
    pub schema: Option<SharedString>,
    pub table: Option<SharedString>,
    pub column: Option<SharedString>,
}

impl ObjectPath {
    pub fn connection(connection: ConnectionId) -> Self {
        Self {
            connection,
            database: None,
            schema: None,
            table: None,
            column: None,
        }
    }

    pub fn database(connection: ConnectionId, database: SharedString) -> Self {
        Self {
            database: Some(database),
            ..Self::connection(connection)
        }
    }

    pub fn schema(connection: ConnectionId, database: SharedString, schema: SharedString) -> Self {
        Self {
            schema: Some(schema),
            ..Self::database(connection, database)
        }
    }

    pub fn table(
        connection: ConnectionId,
        database: SharedString,
        schema: SharedString,
        table: SharedString,
    ) -> Self {
        Self {
            table: Some(table),
            ..Self::schema(connection, database, schema)
        }
    }

    pub fn column(
        connection: ConnectionId,
        database: SharedString,
        schema: SharedString,
        table: SharedString,
        column: SharedString,
    ) -> Self {
        Self {
            column: Some(column),
            ..Self::table(connection, database, schema, table)
        }
    }

    pub fn kind(&self) -> ObjectKind {
        match (&self.database, &self.schema, &self.table, &self.column) {
            (None, ..) => ObjectKind::Connection,
            (Some(_), None, ..) => ObjectKind::Database,
            (Some(_), Some(_), None, _) => ObjectKind::Schema,
            (Some(_), Some(_), Some(_), None) => ObjectKind::Table,
            (Some(_), Some(_), Some(_), Some(_)) => ObjectKind::Column,
        }
    }

    /// `"schema"."table"`, ready to paste into a statement.
    pub fn qualified_table(&self) -> Option<String> {
        let schema = self.schema.as_ref()?;
        let table = self.table.as_ref()?;
        Some(format!(
            "{}.{}",
            quote_identifier(schema),
            quote_identifier(table)
        ))
    }
}

/// A Postgres identifier as SQL, so that a mixed-case or reserved name in a
/// generated statement still means the name it came from.
pub fn quote_identifier(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TableKind {
    Table,
    View,
    MaterializedView,
    ForeignTable,
}

impl TableKind {
    fn from_relkind(relkind: &str) -> Option<Self> {
        match relkind {
            "r" | "p" => Some(TableKind::Table),
            "v" => Some(TableKind::View),
            "m" => Some(TableKind::MaterializedView),
            "f" => Some(TableKind::ForeignTable),
            _ => None,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            TableKind::Table => "table",
            TableKind::View => "view",
            TableKind::MaterializedView => "materialized view",
            TableKind::ForeignTable => "foreign table",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Table {
    pub name: SharedString,
    pub kind: TableKind,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Column {
    pub name: SharedString,
    pub data_type: SharedString,
    pub nullable: bool,
    pub primary_key: bool,
    pub default: Option<SharedString>,
}

impl Column {
    /// The column as it reads in a `CREATE TABLE`, which is also how it is
    /// worth describing to the agent.
    pub fn signature(&self) -> String {
        let mut signature = format!("{} {}", self.name, self.data_type);
        if !self.nullable {
            signature.push_str(" not null");
        }
        if self.primary_key {
            signature.push_str(" primary key");
        }
        if let Some(default) = &self.default {
            signature.push_str(&format!(" default {default}"));
        }
        signature
    }
}

/// Where one level of one connection's catalog has got to.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum Load<T> {
    #[default]
    Unloaded,
    Loading,
    Loaded(T),
    Failed(SharedString),
}

impl<T> Load<T> {
    pub fn loaded(&self) -> Option<&T> {
        match self {
            Load::Loaded(value) => Some(value),
            _ => None,
        }
    }

    pub fn is_loading(&self) -> bool {
        matches!(self, Load::Loading)
    }

    pub fn error(&self) -> Option<&SharedString> {
        match self {
            Load::Failed(error) => Some(error),
            _ => None,
        }
    }
}

/// One connection's catalog, filled in as the tree is opened rather than all
/// at once: a server with many databases, each with many tables, is a lot to
/// ask for before knowing which one is wanted.
#[derive(Default)]
struct Catalog {
    databases: Load<Vec<SharedString>>,
    schemas: HashMap<SharedString, Load<Vec<SharedString>>>,
    tables: HashMap<(SharedString, SharedString), Load<Vec<Table>>>,
    columns: HashMap<(SharedString, SharedString, SharedString), Load<Vec<Column>>>,
}

/// What one statement produced: its rows, or how many rows it changed.
#[derive(Clone, Debug, Default)]
pub struct QueryResult {
    pub columns: Vec<SharedString>,
    pub rows: Vec<Vec<Option<SharedString>>>,
    pub rows_affected: u64,
    /// Whether [`MAX_ROWS`] cut the result short.
    pub truncated: bool,
    pub elapsed: Duration,
}

pub enum DatabaseStoreEvent {
    /// The connections, or some part of some catalog, changed.
    Changed,
}

struct GlobalDatabaseStore(Entity<DatabaseStore>);

impl Global for GlobalDatabaseStore {}

/// A pool shared between everything asking for the same database, held as a
/// task so that two rows opened at once wait on one connection rather than
/// opening two.
type PoolTask = Shared<Task<Result<PgPool, Arc<anyhow::Error>>>>;

pub struct DatabaseStore {
    credentials: Arc<dyn CredentialsProvider>,
    connections: Vec<ConnectionConfig>,
    catalogs: HashMap<ConnectionId, Catalog>,
    expanded: HashSet<ObjectPath>,
    pools: HashMap<(ConnectionId, SharedString), PoolTask>,
    /// Kept so that a load in flight is not cancelled by the task being
    /// dropped; keyed by the node whose children are being loaded.
    loads: HashMap<ObjectPath, Task<()>>,
    next_id: u64,
}

impl EventEmitter<DatabaseStoreEvent> for DatabaseStore {}

impl DatabaseStore {
    pub fn global(cx: &App) -> Option<Entity<Self>> {
        cx.try_global::<GlobalDatabaseStore>()
            .map(|store| store.0.clone())
    }

    fn new(cx: &mut Context<Self>) -> Self {
        let connections = load_connections();
        let next_id = connections
            .iter()
            .map(|connection| connection.id.0 + 1)
            .max()
            .unwrap_or(1);
        Self {
            credentials: zed_credentials_provider::global(cx),
            connections,
            catalogs: HashMap::new(),
            expanded: HashSet::new(),
            pools: HashMap::new(),
            loads: HashMap::new(),
            next_id,
        }
    }

    pub fn connections(&self) -> &[ConnectionConfig] {
        &self.connections
    }

    pub fn connection(&self, id: ConnectionId) -> Option<&ConnectionConfig> {
        self.connections
            .iter()
            .find(|connection| connection.id == id)
    }

    /// Saves a connection, adding it when its id is not one already saved. The
    /// password, when given, replaces whatever is in the keychain; `None`
    /// leaves the stored one alone, so that editing a host does not require
    /// retyping it.
    pub fn save_connection(
        &mut self,
        mut config: ConnectionConfig,
        password: Option<String>,
        cx: &mut Context<Self>,
    ) -> ConnectionId {
        if config.id.0 == 0 {
            config.id = ConnectionId(self.next_id);
            self.next_id += 1;
        } else {
            self.next_id = self.next_id.max(config.id.0 + 1);
        }
        let id = config.id;
        match self
            .connections
            .iter_mut()
            .find(|connection| connection.id == id)
        {
            Some(existing) => *existing = config,
            None => self.connections.push(config),
        }
        // The old pools are to the old host, user or database.
        self.pools.retain(|(pool_id, _), _| *pool_id != id);
        self.catalogs.remove(&id);
        self.persist(cx);

        if let Some(password) = password {
            let credentials = self.credentials.clone();
            cx.spawn(async move |_, cx| {
                let url = credentials_url(id);
                let result = if password.is_empty() {
                    credentials.delete_credentials(&url, cx).await
                } else {
                    credentials
                        .write_credentials(&url, "postgres", password.as_bytes(), cx)
                        .await
                };
                if let Err(error) = result {
                    log::error!("storing a database password in the keychain: {error:#}");
                }
            })
            .detach();
        }

        cx.emit(DatabaseStoreEvent::Changed);
        cx.notify();
        id
    }

    pub fn remove_connection(&mut self, id: ConnectionId, cx: &mut Context<Self>) {
        self.connections.retain(|connection| connection.id != id);
        self.catalogs.remove(&id);
        self.pools.retain(|(pool_id, _), _| *pool_id != id);
        self.expanded.retain(|path| path.connection != id);
        self.persist(cx);

        let credentials = self.credentials.clone();
        cx.spawn(async move |_, cx| {
            if let Err(error) = credentials
                .delete_credentials(&credentials_url(id), cx)
                .await
            {
                log::error!("removing a database password from the keychain: {error:#}");
            }
        })
        .detach();

        cx.emit(DatabaseStoreEvent::Changed);
        cx.notify();
    }

    fn persist(&self, cx: &mut Context<Self>) {
        let connections = self.connections.clone();
        match serde_json::to_string(&connections) {
            Ok(serialized) => db::write_and_log(cx, move || async move {
                db::kvp::GlobalKeyValueStore::global()
                    .write_kvp(CONNECTIONS_KEY.into(), serialized)
                    .await
            }),
            Err(error) => log::error!("serializing the saved database connections: {error:#}"),
        }
    }

    pub fn is_expanded(&self, path: &ObjectPath) -> bool {
        self.expanded.contains(path)
    }

    /// Opens or closes a row, loading its children the first time it opens.
    pub fn toggle_expanded(&mut self, path: ObjectPath, cx: &mut Context<Self>) {
        if self.expanded.remove(&path) {
            cx.emit(DatabaseStoreEvent::Changed);
            cx.notify();
            return;
        }
        self.expanded.insert(path.clone());
        self.load_children(path, false, cx);
    }

    pub fn databases(&self, connection: ConnectionId) -> &Load<Vec<SharedString>> {
        static UNLOADED: Load<Vec<SharedString>> = Load::Unloaded;
        self.catalogs
            .get(&connection)
            .map(|catalog| &catalog.databases)
            .unwrap_or(&UNLOADED)
    }

    pub fn schemas(
        &self,
        connection: ConnectionId,
        database: &SharedString,
    ) -> &Load<Vec<SharedString>> {
        static UNLOADED: Load<Vec<SharedString>> = Load::Unloaded;
        self.catalogs
            .get(&connection)
            .and_then(|catalog| catalog.schemas.get(database))
            .unwrap_or(&UNLOADED)
    }

    pub fn tables(
        &self,
        connection: ConnectionId,
        database: &SharedString,
        schema: &SharedString,
    ) -> &Load<Vec<Table>> {
        static UNLOADED: Load<Vec<Table>> = Load::Unloaded;
        self.catalogs
            .get(&connection)
            .and_then(|catalog| catalog.tables.get(&(database.clone(), schema.clone())))
            .unwrap_or(&UNLOADED)
    }

    pub fn columns(
        &self,
        connection: ConnectionId,
        database: &SharedString,
        schema: &SharedString,
        table: &SharedString,
    ) -> &Load<Vec<Column>> {
        static UNLOADED: Load<Vec<Column>> = Load::Unloaded;
        self.catalogs
            .get(&connection)
            .and_then(|catalog| {
                catalog
                    .columns
                    .get(&(database.clone(), schema.clone(), table.clone()))
            })
            .unwrap_or(&UNLOADED)
    }

    /// Throws away everything known about one connection — its pools included,
    /// so a server restarted underneath us is reconnected to — and asks again
    /// for every level the tree currently has open. Reloading only the top
    /// would leave the rows below it saying "Loading…" until each was clicked.
    pub fn refresh_connection(&mut self, connection: ConnectionId, cx: &mut Context<Self>) {
        self.pools.retain(|(id, _), _| *id != connection);
        self.catalogs.remove(&connection);
        let open: Vec<ObjectPath> = self
            .expanded
            .iter()
            .filter(|path| path.connection == connection)
            .cloned()
            .collect();
        // Each level is its own query against the catalog, so they do not have
        // to wait for one another.
        for path in open {
            self.load_children(path, true, cx);
        }
        cx.emit(DatabaseStoreEvent::Changed);
        cx.notify();
    }

    /// Throws away what is known about a node's children and asks again.
    pub fn refresh(&mut self, path: ObjectPath, cx: &mut Context<Self>) {
        if path.kind() == ObjectKind::Connection {
            self.pools.retain(|(id, _), _| *id != path.connection);
        }
        self.load_children(path, true, cx);
    }

    /// Loads a node's children unless they are already there. `force` asks
    /// again even so, which is what the Refresh entry of a menu wants.
    fn load_children(&mut self, path: ObjectPath, force: bool, cx: &mut Context<Self>) {
        if path.column.is_some() {
            return;
        }
        let connection = path.connection;
        let database = path.database.clone();
        let schema = path.schema.clone();
        let table = path.table.clone();

        // The database a metadata query runs against: the one being opened, or
        // the connection's own when the tree is still at the connection. Read
        // before the catalog is borrowed below.
        let against = database.clone().unwrap_or_else(|| {
            self.connection(connection)
                .map(|config| SharedString::from(config.database.clone()))
                .unwrap_or_default()
        });

        let catalog = self.catalogs.entry(connection).or_default();
        let slot: &mut dyn LoadSlot = match (&database, &schema, &table) {
            (None, ..) => &mut catalog.databases,
            (Some(database), None, _) => catalog.schemas.entry(database.clone()).or_default(),
            (Some(database), Some(schema), None) => catalog
                .tables
                .entry((database.clone(), schema.clone()))
                .or_default(),
            (Some(database), Some(schema), Some(table)) => catalog
                .columns
                .entry((database.clone(), schema.clone(), table.clone()))
                .or_default(),
        };
        if !force && !matches!(slot.state(), LoadState::Unloaded | LoadState::Failed) {
            return;
        }
        slot.set_loading();

        let pool = self.pool(connection, against, cx);

        let load = cx.spawn({
            let path = path.clone();
            async move |this, cx| {
                let loaded = load_catalog_level(pool, path.clone(), cx).await;
                this.update(cx, |this, cx| {
                    this.store_children(&path, loaded);
                    cx.emit(DatabaseStoreEvent::Changed);
                    cx.notify();
                })
                .log_err();
            }
        });
        self.loads.insert(path, load);
        cx.emit(DatabaseStoreEvent::Changed);
        cx.notify();
    }

    fn store_children(&mut self, path: &ObjectPath, loaded: Result<CatalogLevel>) {
        let catalog = self.catalogs.entry(path.connection).or_default();
        match (loaded, &path.database, &path.schema, &path.table) {
            (Ok(CatalogLevel::Databases(databases)), None, ..) => {
                catalog.databases = Load::Loaded(databases);
            }
            (Ok(CatalogLevel::Schemas(schemas)), Some(database), None, _) => {
                catalog
                    .schemas
                    .insert(database.clone(), Load::Loaded(schemas));
            }
            (Ok(CatalogLevel::Tables(tables)), Some(database), Some(schema), None) => {
                catalog
                    .tables
                    .insert((database.clone(), schema.clone()), Load::Loaded(tables));
            }
            (Ok(CatalogLevel::Columns(columns)), Some(database), Some(schema), Some(table)) => {
                catalog.columns.insert(
                    (database.clone(), schema.clone(), table.clone()),
                    Load::Loaded(columns),
                );
            }
            (Err(error), ..) => {
                let error = SharedString::from(format!("{error:#}"));
                match (&path.database, &path.schema, &path.table) {
                    (None, ..) => catalog.databases = Load::Failed(error),
                    (Some(database), None, _) => {
                        catalog
                            .schemas
                            .insert(database.clone(), Load::Failed(error));
                    }
                    (Some(database), Some(schema), None) => {
                        catalog
                            .tables
                            .insert((database.clone(), schema.clone()), Load::Failed(error));
                    }
                    (Some(database), Some(schema), Some(table)) => {
                        catalog.columns.insert(
                            (database.clone(), schema.clone(), table.clone()),
                            Load::Failed(error),
                        );
                    }
                }
            }
            // `load_catalog_level` answers the level it was asked about, so
            // the remaining pairings cannot happen.
            (Ok(_), ..) => log::error!("a catalog load answered about the wrong level"),
        }
    }

    /// The pool for one database of one connection, opened the first time it
    /// is asked for.
    fn pool(
        &mut self,
        connection: ConnectionId,
        database: SharedString,
        cx: &mut Context<Self>,
    ) -> PoolTask {
        let key = (connection, database.clone());
        if let Some(pool) = self.pools.get(&key) {
            return pool.clone();
        }
        let config = self.connection(connection).cloned();
        let credentials = self.credentials.clone();
        let pool = cx
            .spawn(async move |_, cx| {
                connect(config, database, credentials, cx)
                    .await
                    .map_err(Arc::new)
            })
            .shared();
        self.pools.insert(key, pool.clone());
        pool
    }

    /// Runs one or more statements against a database, for the SQL console.
    pub fn execute(
        &mut self,
        connection: ConnectionId,
        database: SharedString,
        sql: String,
        cx: &mut Context<Self>,
    ) -> Task<Result<QueryResult>> {
        let pool = self.pool(connection, database, cx);
        cx.spawn(async move |_, cx| {
            let pool = pool.await.map_err(|error| anyhow!("{error:#}"))?;
            gpui_tokio::Tokio::spawn_result(cx, async move { postgres::execute(&pool, sql).await })
                .await
        })
    }

    /// Reaches a server with settings that have not been saved, and says what
    /// version answered. Nothing is kept: no pool, no keychain entry — this is
    /// the modal's Test button, which has to work before there is anything to
    /// keep.
    ///
    /// `password` is what was typed. `None` means fall back to the one already
    /// in the keychain, which is what testing an edit without retyping it
    /// should do.
    pub fn test_connection(
        &self,
        config: ConnectionConfig,
        password: Option<String>,
        cx: &mut Context<Self>,
    ) -> Task<Result<SharedString>> {
        let credentials = self.credentials.clone();
        cx.spawn(async move |_, cx| {
            let password = match password {
                Some(password) => Some(password),
                None => stored_password(&credentials, config.id, cx).await?,
            };
            let pool =
                open_pool(&config, &config.database.clone(), password.as_deref(), cx).await?;
            let version =
                gpui_tokio::Tokio::spawn_result(cx, async move { postgres::version(&pool).await })
                    .await?;
            Ok(version)
        })
    }
}

/// The three states [`DatabaseStore::load_children`] cares about, so that it
/// can treat every level of the catalog the same way.
enum LoadState {
    Unloaded,
    Loading,
    Loaded,
    Failed,
}

trait LoadSlot {
    fn state(&self) -> LoadState;
    fn set_loading(&mut self);
}

impl<T> LoadSlot for Load<T> {
    fn state(&self) -> LoadState {
        match self {
            Load::Unloaded => LoadState::Unloaded,
            Load::Loading => LoadState::Loading,
            Load::Loaded(_) => LoadState::Loaded,
            Load::Failed(_) => LoadState::Failed,
        }
    }

    fn set_loading(&mut self) {
        *self = Load::Loading;
    }
}

enum CatalogLevel {
    Databases(Vec<SharedString>),
    Schemas(Vec<SharedString>),
    Tables(Vec<Table>),
    Columns(Vec<Column>),
}

async fn load_catalog_level(
    pool: PoolTask,
    path: ObjectPath,
    cx: &mut AsyncApp,
) -> Result<CatalogLevel> {
    let pool = pool.await.map_err(|error| anyhow!("{error:#}"))?;
    gpui_tokio::Tokio::spawn_result(cx, async move {
        match (&path.database, &path.schema, &path.table) {
            (None, ..) => Ok(CatalogLevel::Databases(postgres::databases(&pool).await?)),
            (Some(_), None, _) => Ok(CatalogLevel::Schemas(postgres::schemas(&pool).await?)),
            (Some(_), Some(schema), None) => {
                Ok(CatalogLevel::Tables(postgres::tables(&pool, schema).await?))
            }
            (Some(_), Some(schema), Some(table)) => Ok(CatalogLevel::Columns(
                postgres::columns(&pool, schema, table).await?,
            )),
        }
    })
    .await
}

async fn connect(
    config: Option<ConnectionConfig>,
    database: SharedString,
    credentials: Arc<dyn CredentialsProvider>,
    cx: &mut AsyncApp,
) -> Result<PgPool> {
    let config = config.context("the connection has been removed")?;
    let password = stored_password(&credentials, config.id, cx).await?;
    let database = if database.is_empty() {
        SharedString::from(config.database.clone())
    } else {
        database
    };
    open_pool(&config, &database, password.as_deref(), cx).await
}

/// The password the keychain holds for a connection, if it holds one.
async fn stored_password(
    credentials: &Arc<dyn CredentialsProvider>,
    id: ConnectionId,
    cx: &AsyncApp,
) -> Result<Option<String>> {
    credentials
        .read_credentials(&credentials_url(id), cx)
        .await
        .context("reading the password from the keychain")?
        .map(|(_, password)| String::from_utf8(password))
        .transpose()
        .context("the password in the keychain is not valid text")
}

async fn open_pool(
    config: &ConnectionConfig,
    database: &str,
    password: Option<&str>,
    cx: &mut AsyncApp,
) -> Result<PgPool> {
    let mut options = PgConnectOptions::new()
        .host(&config.host)
        .port(config.port)
        .username(&config.user)
        .database(database)
        .ssl_mode(config.ssl_mode.into());
    if let Some(password) = password {
        options = options.password(password);
    }

    gpui_tokio::Tokio::spawn_result(cx, async move {
        PgPoolOptions::new()
            .max_connections(4)
            .acquire_timeout(CONNECT_TIMEOUT)
            .connect_with(options)
            .await
            .context("connecting to Postgres")
    })
    .await
}

fn load_connections() -> Vec<ConnectionConfig> {
    let stored = db::kvp::GlobalKeyValueStore::global()
        .read_kvp(CONNECTIONS_KEY)
        .log_err()
        .flatten();
    let Some(stored) = stored else {
        return Vec::new();
    };
    match serde_json::from_str(&stored) {
        Ok(connections) => connections,
        Err(error) => {
            log::error!("reading the saved database connections: {error:#}");
            Vec::new()
        }
    }
}

/// The action that puts text in the composer of the worktree's agent
/// terminal, with a trailing newline so what follows starts its own line.
pub fn send_to_agent(text: impl Into<String>) -> zed_actions::claude::SendText {
    let mut text = text.into();
    text.push('\n');
    zed_actions::claude::SendText { text }
}

/// What "Send to Agent" says about a row of the tree: enough for the agent to
/// find the thing again — which server, which database — and, for a table,
/// what its columns are, since that is what a question about a table is
/// usually about.
pub fn describe(store: &DatabaseStore, path: &ObjectPath) -> String {
    let Some(config) = store.connection(path.connection) else {
        return String::new();
    };
    let where_it_is = format!("connection `{}` ({})", config.name, config.url());
    match (&path.database, &path.schema, &path.table, &path.column) {
        (None, ..) => format!("Postgres {where_it_is}"),
        (Some(database), None, ..) => {
            format!("Postgres database `{database}` on {where_it_is}")
        }
        (Some(database), Some(schema), None, _) => {
            format!("Postgres schema `{schema}` in database `{database}` on {where_it_is}")
        }
        (Some(database), Some(schema), Some(table), None) => {
            let kind = store
                .tables(path.connection, database, schema)
                .loaded()
                .and_then(|tables| tables.iter().find(|candidate| &candidate.name == table))
                .map_or("table", |table| table.kind.label());
            let mut described = format!(
                "Postgres {kind} `{schema}.{table}` in database `{database}` on {where_it_is}"
            );
            match store
                .columns(path.connection, database, schema, table)
                .loaded()
            {
                Some(columns) if !columns.is_empty() => {
                    described.push_str(":\n");
                    for column in columns {
                        described.push_str(&format!("- {}\n", column.signature()));
                    }
                    described.pop();
                }
                _ => described.push('.'),
            }
            described
        }
        (Some(database), Some(schema), Some(table), Some(column)) => {
            let described = store
                .columns(path.connection, database, schema, table)
                .loaded()
                .and_then(|columns| columns.iter().find(|candidate| &candidate.name == column))
                .map_or_else(|| column.to_string(), Column::signature);
            format!(
                "Postgres column `{schema}.{table}.{column}` ({described}) \
                 in database `{database}` on {where_it_is}"
            )
        }
    }
}

/// Several rows of the tree at once, in the order they are shown, each
/// described as [`describe`] would describe it alone.
pub fn describe_all(store: &DatabaseStore, paths: &[ObjectPath]) -> String {
    paths
        .iter()
        .map(|path| describe(store, path))
        .filter(|described| !described.is_empty())
        .collect::<Vec<_>>()
        .join("\n\n")
}

/// The rows a selection covers, as the agent would need them: one row reads
/// better named field by field, several read better as a table, where the
/// column names are said once and the values line up for comparison.
pub fn describe_rows(
    heading: &str,
    columns: &[SharedString],
    rows: &[Vec<Option<SharedString>>],
) -> String {
    let cell = |values: &[Option<SharedString>], index: usize| match values.get(index) {
        Some(Some(value)) => value.to_string(),
        _ => "NULL".to_owned(),
    };

    let mut described = format!("{heading}\n");
    match rows {
        [] => {}
        [values] => {
            for (index, column) in columns.iter().enumerate() {
                described.push_str(&format!("- {column}: {}\n", cell(values, index)));
            }
        }
        rows => {
            described.push_str(&format!("| {} |\n", columns.join(" | ")));
            described.push_str(&format!("| {} |\n", vec!["---"; columns.len()].join(" | ")));
            for values in rows {
                let cells: Vec<String> = (0..columns.len())
                    // A newline or a pipe inside a value would break the row
                    // apart; the agent is better served by seeing it escaped.
                    .map(|index| cell(values, index).replace('\n', " ").replace('|', "\\|"))
                    .collect();
                described.push_str(&format!("| {} |\n", cells.join(" | ")));
            }
        }
    }
    described.pop();
    described
}

/// Postgres itself. Everything sqlx-shaped is in here; the rest of the crate
/// talks in [`Table`], [`Column`] and [`QueryResult`].
mod postgres {
    use super::*;
    use sqlx::{Column as _, Either, Row as _, ValueRef as _};

    /// What the server calls itself — `17.6`, say. `version()` says that and
    /// a great deal more about the build; the setting is the part a Test
    /// button has room for.
    pub(super) async fn version(pool: &PgPool) -> Result<SharedString> {
        let row = sqlx::query("SELECT current_setting('server_version')::text")
            .fetch_one(pool)
            .await
            .context("asking the server its version")?;
        Ok(SharedString::from(row.try_get::<String, _>(0)?))
    }

    /// Every database that can be connected to. Templates cannot be browsed
    /// meaningfully, so they are left out.
    pub(super) async fn databases(pool: &PgPool) -> Result<Vec<SharedString>> {
        let rows = sqlx::query(
            "SELECT datname::text FROM pg_database \
             WHERE datallowconn AND NOT datistemplate ORDER BY datname",
        )
        .fetch_all(pool)
        .await
        .context("listing databases")?;
        rows.into_iter()
            .map(|row| Ok(SharedString::from(row.try_get::<String, _>(0)?)))
            .collect()
    }

    /// Every schema but Postgres's own scratch ones: `pg_toast` and the
    /// per-session `pg_temp`, which hold nothing anyone wrote.
    pub(super) async fn schemas(pool: &PgPool) -> Result<Vec<SharedString>> {
        let rows = sqlx::query(
            "SELECT nspname::text FROM pg_namespace \
             WHERE nspname NOT LIKE 'pg\\_toast%' AND nspname NOT LIKE 'pg\\_temp%' \
             ORDER BY nspname",
        )
        .fetch_all(pool)
        .await
        .context("listing schemas")?;
        rows.into_iter()
            .map(|row| Ok(SharedString::from(row.try_get::<String, _>(0)?)))
            .collect()
    }

    pub(super) async fn tables(pool: &PgPool, schema: &str) -> Result<Vec<Table>> {
        let rows = sqlx::query(
            "SELECT c.relname::text, c.relkind::text \
             FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace \
             WHERE n.nspname = $1 AND c.relkind IN ('r', 'p', 'v', 'm', 'f') \
             ORDER BY c.relname",
        )
        .bind(schema)
        .fetch_all(pool)
        .await
        .context("listing tables")?;
        let mut tables = Vec::with_capacity(rows.len());
        for row in rows {
            let name: String = row.try_get(0)?;
            let relkind: String = row.try_get(1)?;
            if let Some(kind) = TableKind::from_relkind(&relkind) {
                tables.push(Table {
                    name: name.into(),
                    kind,
                });
            }
        }
        Ok(tables)
    }

    pub(super) async fn columns(pool: &PgPool, schema: &str, table: &str) -> Result<Vec<Column>> {
        let rows = sqlx::query(
            "SELECT a.attname::text, \
                    format_type(a.atttypid, a.atttypmod)::text, \
                    (NOT a.attnotnull)::text, \
                    (EXISTS ( \
                        SELECT 1 FROM pg_index i \
                        WHERE i.indrelid = c.oid AND i.indisprimary \
                          AND a.attnum = ANY(i.indkey) \
                    ))::text, \
                    pg_get_expr(d.adbin, d.adrelid)::text \
             FROM pg_attribute a \
             JOIN pg_class c ON c.oid = a.attrelid \
             JOIN pg_namespace n ON n.oid = c.relnamespace \
             LEFT JOIN pg_attrdef d ON d.adrelid = c.oid AND d.adnum = a.attnum \
             WHERE n.nspname = $1 AND c.relname = $2 \
               AND a.attnum > 0 AND NOT a.attisdropped \
             ORDER BY a.attnum",
        )
        .bind(schema)
        .bind(table)
        .fetch_all(pool)
        .await
        .context("listing columns")?;
        let mut columns = Vec::with_capacity(rows.len());
        for row in rows {
            let name: String = row.try_get(0)?;
            let data_type: String = row.try_get(1)?;
            let nullable: String = row.try_get(2)?;
            let primary_key: String = row.try_get(3)?;
            let default: Option<String> = row.try_get(4)?;
            columns.push(Column {
                name: name.into(),
                data_type: data_type.into(),
                nullable: nullable == "true",
                primary_key: primary_key == "true",
                default: default.map(SharedString::from),
            });
        }
        Ok(columns)
    }

    /// Runs whatever was typed. [`sqlx::raw_sql`] sends it over the simple
    /// query protocol, which answers in text for every type — that is what
    /// lets a result be shown without a decoder for each of Postgres's types,
    /// the extensions' included.
    pub(super) async fn execute(pool: &PgPool, sql: String) -> Result<QueryResult> {
        use futures::StreamExt as _;

        let started = Instant::now();
        let mut result = QueryResult::default();
        let mut stream = sqlx::raw_sql(&sql).fetch_many(pool);
        while let Some(next) = stream.next().await {
            match next.context("running the statement")? {
                Either::Left(outcome) => result.rows_affected += outcome.rows_affected(),
                Either::Right(row) => {
                    if result.columns.is_empty() {
                        result.columns = row
                            .columns()
                            .iter()
                            .map(|column| SharedString::from(column.name().to_owned()))
                            .collect();
                    }
                    if result.rows.len() >= MAX_ROWS {
                        result.truncated = true;
                        continue;
                    }
                    let mut values = Vec::with_capacity(result.columns.len());
                    for index in 0..row.columns().len() {
                        let value = row.try_get_raw(index)?;
                        values.push(if value.is_null() {
                            None
                        } else {
                            Some(SharedString::from(
                                value
                                    .as_str()
                                    .map(str::to_owned)
                                    .unwrap_or_else(|_| "<binary>".to_owned()),
                            ))
                        });
                    }
                    result.rows.push(values);
                }
            }
        }
        result.elapsed = started.elapsed();
        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identifiers_survive_quoting() {
        assert_eq!(quote_identifier("users"), "\"users\"");
        assert_eq!(quote_identifier("Mixed Case"), "\"Mixed Case\"");
        assert_eq!(quote_identifier("we\"ird"), "\"we\"\"ird\"");
    }

    #[test]
    fn a_path_knows_what_it_points_at() {
        let id = ConnectionId(1);
        assert_eq!(ObjectPath::connection(id).kind(), ObjectKind::Connection);
        assert_eq!(
            ObjectPath::database(id, "app".into()).kind(),
            ObjectKind::Database
        );
        assert_eq!(
            ObjectPath::schema(id, "app".into(), "public".into()).kind(),
            ObjectKind::Schema
        );
        let table = ObjectPath::table(id, "app".into(), "public".into(), "users".into());
        assert_eq!(table.kind(), ObjectKind::Table);
        assert_eq!(
            table.qualified_table().as_deref(),
            Some("\"public\".\"users\"")
        );
        assert_eq!(
            ObjectPath::column(
                id,
                "app".into(),
                "public".into(),
                "users".into(),
                "id".into()
            )
            .kind(),
            ObjectKind::Column
        );
    }

    /// What the console shows depends on Postgres answering the simple query
    /// protocol in text for every type, which only a real server can say.
    /// Point `BENCH_POSTGRES_URL` at one and run:
    ///
    /// ```text
    /// cargo test -p database_explorer -- --ignored
    /// ```
    #[test]
    #[ignore = "needs a Postgres server at BENCH_POSTGRES_URL"]
    fn every_type_comes_back_as_text() -> Result<()> {
        let url = std::env::var("BENCH_POSTGRES_URL")?;
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()?;
        runtime.block_on(async move {
            let pool = PgPoolOptions::new()
                .max_connections(1)
                .connect(&url)
                .await?;

            let version = postgres::version(&pool).await?;
            assert!(
                version
                    .chars()
                    .next()
                    .is_some_and(|first| first.is_ascii_digit()),
                "a version reads like `17.6`, got {version:?}"
            );

            let databases = postgres::databases(&pool).await?;
            assert!(!databases.is_empty(), "a server has at least one database");
            let schemas = postgres::schemas(&pool).await?;
            assert!(schemas.iter().any(|schema| schema == "public"));

            let result = postgres::execute(
                &pool,
                "SELECT 1::int AS n, 'x'::text AS t, true AS b, \
                 '2020-01-02'::date AS d, '{\"a\":1}'::jsonb AS j, NULL::int AS nothing"
                    .to_owned(),
            )
            .await?;
            assert_eq!(
                result.columns,
                vec!["n", "t", "b", "d", "j", "nothing"]
                    .into_iter()
                    .map(SharedString::from)
                    .collect::<Vec<_>>()
            );
            assert_eq!(
                result.rows,
                vec![vec![
                    Some("1".into()),
                    Some("x".into()),
                    Some("t".into()),
                    Some("2020-01-02".into()),
                    Some("{\"a\": 1}".into()),
                    None,
                ]]
            );

            postgres::execute(
                &pool,
                "CREATE SCHEMA bench_probe; \
                 CREATE TABLE bench_probe.people ( \
                     id integer PRIMARY KEY, \
                     email text NOT NULL, \
                     joined date DEFAULT now() \
                 ); \
                 CREATE VIEW bench_probe.everyone AS SELECT * FROM bench_probe.people;"
                    .to_owned(),
            )
            .await?;

            let tables = postgres::tables(&pool, "bench_probe").await?;
            assert_eq!(
                tables,
                vec![
                    Table {
                        name: "everyone".into(),
                        kind: TableKind::View,
                    },
                    Table {
                        name: "people".into(),
                        kind: TableKind::Table,
                    },
                ]
            );

            let columns = postgres::columns(&pool, "bench_probe", "people").await?;
            let signatures: Vec<_> = columns.iter().map(Column::signature).collect();
            assert_eq!(
                signatures,
                vec![
                    "id integer not null primary key",
                    "email text not null",
                    "joined date default now()",
                ]
            );

            postgres::execute(&pool, "DROP SCHEMA bench_probe CASCADE".to_owned()).await?;

            anyhow::Ok(())
        })
    }

    #[test]
    fn one_row_is_named_field_by_field() {
        let columns: Vec<SharedString> = vec!["id".into(), "email".into()];
        let rows = vec![vec![Some("1".into()), None]];
        assert_eq!(
            describe_rows("Row 1:", &columns, &rows),
            "Row 1:\n- id: 1\n- email: NULL"
        );
    }

    #[test]
    fn several_rows_line_up_as_a_table() {
        let columns: Vec<SharedString> = vec!["id".into(), "note".into()];
        let rows = vec![
            vec![Some("1".into()), Some("a | b".into())],
            vec![Some("2".into()), Some("two\nlines".into())],
        ];
        assert_eq!(
            describe_rows("2 rows:", &columns, &rows),
            "2 rows:\n\
             | id | note |\n\
             | --- | --- |\n\
             | 1 | a \\| b |\n\
             | 2 | two lines |"
        );
    }

    #[test]
    fn a_column_reads_as_it_was_declared() {
        let column = Column {
            name: "id".into(),
            data_type: "integer".into(),
            nullable: false,
            primary_key: true,
            default: Some("nextval('users_id_seq')".into()),
        };
        assert_eq!(
            column.signature(),
            "id integer not null primary key default nextval('users_id_seq')"
        );
    }
}
