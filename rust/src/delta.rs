//! Delta Table read and write implementation

// Reference: https://github.com/delta-io/delta/blob/master/PROTOCOL.md

use std::cmp::Ordering;
use std::collections::HashMap;
use std::fmt;
use std::io::{BufRead, BufReader, Cursor};

use arrow::error::ArrowError;
use chrono::{DateTime, FixedOffset, Utc};
use futures::StreamExt;
use lazy_static::lazy_static;
use log::debug;
use parquet::errors::ParquetError;
use parquet::file::{
    reader::{FileReader, SerializedFileReader},
    serialized_reader::SliceableCursor,
};
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::convert::TryFrom;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use super::action;
use super::action::{Action, DeltaOperation};
use super::partitions::{DeltaTablePartition, PartitionFilter};
use super::schema::*;
use super::storage;
use super::storage::{StorageBackend, StorageError, UriError};
use uuid::Uuid;

/// Metadata for a checkpoint file
#[derive(Serialize, Deserialize, Debug, Default, Clone, Copy)]
pub struct CheckPoint {
    /// Delta table version
    version: DeltaDataTypeVersion, // 20 digits decimals
    size: DeltaDataTypeLong,
    parts: Option<u32>, // 10 digits decimals
}

impl PartialEq for CheckPoint {
    fn eq(&self, other: &Self) -> bool {
        self.version == other.version
    }
}

impl Eq for CheckPoint {}

/// Delta Table specific error
#[derive(thiserror::Error, Debug)]
pub enum DeltaTableError {
    /// Error returned when applying transaction log failed.
    #[error("Failed to apply transaction log: {}", .source)]
    ApplyLog {
        /// Apply error details returned when applying transaction log failed.
        #[from]
        source: ApplyLogError,
    },
    /// Error returned when loading checkpoint failed.
    #[error("Failed to load checkpoint: {}", .source)]
    LoadCheckpoint {
        /// Load checkpoint error details returned when loading checkpoint failed.
        #[from]
        source: LoadCheckpointError,
    },
    /// Error returned when reading the delta log object failed.
    #[error("Failed to read delta log object: {}", .source)]
    StorageError {
        /// Storage error details when reading the delta log object failed.
        #[from]
        source: StorageError,
    },
    /// Error returned when reading the checkpoint failed.
    #[error("Failed to read checkpoint: {}", .source)]
    ParquetError {
        /// Parquet error details returned when reading the checkpoint failed.
        #[from]
        source: ParquetError,
    },
    /// Error returned when converting the schema in Arrow format failed.
    #[error("Failed to convert into Arrow schema: {}", .source)]
    ArrowError {
        /// Arrow error details returned when converting the schema in Arrow format failed
        #[from]
        source: ArrowError,
    },
    /// Error returned when the table has an invalid path.
    #[error("Invalid table path: {}", .source)]
    UriError {
        /// Uri error details returned when the table has an invalid path.
        #[from]
        source: UriError,
    },
    /// Error returned when the log record has an invalid JSON.
    #[error("Invalid JSON in log record: {}", .source)]
    InvalidJson {
        /// JSON error details returned when the log record has an invalid JSON.
        #[from]
        source: serde_json::error::Error,
    },
    /// Error returned when the DeltaTable has an invalid version.
    #[error("Invalid table version: {0}")]
    InvalidVersion(DeltaDataTypeVersion),
    /// Error returned when the DeltaTable has no data files.
    #[error("Corrupted table, cannot read data file {}: {}", .path, .source)]
    MissingDataFile {
        /// Source error details returned when the DeltaTable has no data files.
        source: std::io::Error,
        /// The Path used of the DeltaTable
        path: String,
    },
    /// Error returned when the datetime string is invalid for a conversion.
    #[error("Invalid datetime string: {}", .source)]
    InvalidDateTimeString {
        /// Parse error details returned of the datetime string parse error.
        #[from]
        source: chrono::ParseError,
    },
    /// Error returned when the action record is invalid in log.
    #[error("Invalid action record found in log: {}", .source)]
    InvalidAction {
        /// Action error details returned of the invalid action.
        #[from]
        source: action::ActionError,
    },
    /// Error returned when it is not a DeltaTable.
    #[error("Not a Delta table")]
    NotATable,
    /// Error returned when no metadata was found in the DeltaTable.
    #[error("No metadata found, please make sure table is loaded.")]
    NoMetadata,
    /// Error returned when no schema was found in the DeltaTable.
    #[error("No schema found, please make sure table is loaded.")]
    NoSchema,
    /// Error returned when no partition was found in the DeltaTable.
    #[error("No partitions found, please make sure table is partitioned.")]
    LoadPartitions,
    /// Error returned when a partition is not formatted as a Hive Partition.
    #[error("This partition is not formatted with key=value: {}", .partition)]
    PartitionError {
        /// The malformed partition used.
        partition: String,
    },
    /// Error returned when a invalid partition filter was found.
    #[error("Invalid partition filter found: {}.", .partition_filter)]
    InvalidPartitionFilter {
        /// The invalid partition filter used.
        partition_filter: String,
    },
    /// Error returned when Vacuume retention period is below the safe threshold
    #[error(
        "Invalid retention period, retention for Vacuum must be greater than 1 week (168 hours)"
    )]
    InvalidVacuumRetentionPeriod,
}

/// Delta table metadata
#[derive(Clone, Debug)]
pub struct DeltaTableMetaData {
    /// Unique identifier for this table
    pub id: Guid,
    /// User-provided identifier for this table
    pub name: Option<String>,
    /// User-provided description for this table
    pub description: Option<String>,
    /// Specification of the encoding for the files stored in the table
    pub format: action::Format,
    /// Schema of the table
    pub schema: Schema,
    /// An array containing the names of columns by which the data should be partitioned
    pub partition_columns: Vec<String>,
    /// The time when this metadata action is created, in milliseconds since the Unix epoch
    pub created_time: DeltaDataTypeTimestamp,
    /// table properties
    pub configuration: HashMap<String, String>,
}

impl fmt::Display for DeltaTableMetaData {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(
            f,
            "GUID={}, name={:?}, description={:?}, partitionColumns={:?}, createdTime={:?}, configuration={:?}",
            self.id, self.name, self.description, self.partition_columns, self.created_time, self.configuration
        )
    }
}

/// Error related to Delta log application
#[derive(thiserror::Error, Debug)]
pub enum ApplyLogError {
    /// Error returned when the end of transaction log is reached.
    #[error("End of transaction log")]
    EndOfLog,
    /// Error returned when the JSON of the log record is invalid.
    #[error("Invalid JSON in log record")]
    InvalidJson {
        /// JSON error details returned when reading the JSON log record.
        #[from]
        source: serde_json::error::Error,
    },
    /// Error returned when the storage failed to read the log content.
    #[error("Failed to read log content")]
    Storage {
        /// Storage error details returned while reading the log content.
        source: StorageError,
    },
    /// Error returned when a line from log record is invalid.
    #[error("Failed to read line from log record")]
    Io {
        /// Source error details returned while reading the log record.
        #[from]
        source: std::io::Error,
    },
}

impl From<StorageError> for ApplyLogError {
    fn from(error: StorageError) -> Self {
        match error {
            StorageError::NotFound => ApplyLogError::EndOfLog,
            _ => ApplyLogError::Storage { source: error },
        }
    }
}

/// Error related to checkpoint loading
#[derive(thiserror::Error, Debug)]
pub enum LoadCheckpointError {
    /// Error returned when the JSON checkpoint is not found.
    #[error("Checkpoint file not found")]
    NotFound,
    /// Error returned when the JSON checkpoint is invalid.
    #[error("Invalid JSON in checkpoint: {source}")]
    InvalidJson {
        /// Error details returned while reading the JSON.
        #[from]
        source: serde_json::error::Error,
    },
    /// Error returned when it failed to read the checkpoint content.
    #[error("Failed to read checkpoint content: {source}")]
    Storage {
        /// Storage error details returned while reading the checkpoint content.
        source: StorageError,
    },
}

impl From<StorageError> for LoadCheckpointError {
    fn from(error: StorageError) -> Self {
        match error {
            StorageError::NotFound => LoadCheckpointError::NotFound,
            _ => LoadCheckpointError::Storage { source: error },
        }
    }
}

#[derive(Default, Debug)]
struct DeltaTableState {
    // A remove action should remain in the state of the table as a tombstone until it has expired.
    // A tombstone expires when the creation timestamp of the delta file exceeds the expiration
    tombstones: Vec<action::Remove>,
    files: Vec<action::Add>,
    commit_infos: Vec<Value>,
    app_transaction_version: HashMap<String, DeltaDataTypeVersion>,
    min_reader_version: i32,
    min_writer_version: i32,
    current_metadata: Option<DeltaTableMetaData>,
}

/// In memory representation of a Delta Table
pub struct DeltaTable {
    /// The version of the table as of the most recent loaded Delta log entry.
    pub version: DeltaDataTypeVersion,
    /// The path the DeltaTable was loaded from.
    pub table_path: String,

    state: DeltaTableState,

    // metadata
    // application_transactions
    storage: Box<dyn StorageBackend>,

    last_check_point: Option<CheckPoint>,
    log_path: String,
    version_timestamp: HashMap<DeltaDataTypeVersion, i64>,
}

impl DeltaTable {
    fn version_to_log_path(&self, version: DeltaDataTypeVersion) -> String {
        let version = format!("{:020}.json", version);
        self.storage.join_path(&self.log_path, &version)
    }

    fn tmp_commit_log_path(&self, token: &str) -> String {
        let path = format!("_commit_{}.json", token);
        self.storage.join_path(&self.log_path, &path)
    }

    fn get_checkpoint_data_paths(&self, check_point: &CheckPoint) -> Vec<String> {
        let checkpoint_prefix_pattern = format!("{:020}", check_point.version);
        let checkpoint_prefix = self
            .storage
            .join_path(&self.log_path, &checkpoint_prefix_pattern);
        let mut checkpoint_data_paths = Vec::new();

        match check_point.parts {
            None => {
                checkpoint_data_paths.push(format!("{}.checkpoint.parquet", checkpoint_prefix));
            }
            Some(parts) => {
                for i in 0..parts {
                    checkpoint_data_paths.push(format!(
                        "{}.checkpoint.{:010}.{:010}.parquet",
                        checkpoint_prefix,
                        i + 1,
                        parts
                    ));
                }
            }
        }

        checkpoint_data_paths
    }

    async fn get_last_checkpoint(&self) -> Result<CheckPoint, LoadCheckpointError> {
        let last_checkpoint_path = self.storage.join_path(&self.log_path, "_last_checkpoint");
        let data = self.storage.get_obj(&last_checkpoint_path).await?;

        Ok(serde_json::from_slice(&data)?)
    }

    async fn find_latest_check_point_for_version(
        &self,
        version: DeltaDataTypeVersion,
    ) -> Result<Option<CheckPoint>, DeltaTableError> {
        lazy_static! {
            static ref CHECKPOINT_REGEX: Regex =
                Regex::new(r#"^*[/\\]_delta_log[/\\](\d{20})\.checkpoint\.parquet$"#).unwrap();
            static ref CHECKPOINT_PARTS_REGEX: Regex = Regex::new(
                r#"^*[/\\]_delta_log[/\\](\d{20})\.checkpoint\.\d{10}\.(\d{10})\.parquet$"#
            )
            .unwrap();
        }

        let mut cp: Option<CheckPoint> = None;
        let mut stream = self.storage.list_objs(&self.log_path).await?;

        while let Some(obj_meta) = stream.next().await {
            // Exit early if any objects can't be listed.
            let obj_meta = obj_meta?;
            if let Some(captures) = CHECKPOINT_REGEX.captures(&obj_meta.path) {
                let curr_ver_str = captures.get(1).unwrap().as_str();
                let curr_ver: DeltaDataTypeVersion = curr_ver_str.parse().unwrap();
                if curr_ver > version {
                    // skip checkpoints newer than max version
                    continue;
                }
                if cp.is_none() || curr_ver > cp.unwrap().version {
                    cp = Some(CheckPoint {
                        version: curr_ver,
                        size: 0,
                        parts: None,
                    });
                }
                continue;
            }

            if let Some(captures) = CHECKPOINT_PARTS_REGEX.captures(&obj_meta.path) {
                let curr_ver_str = captures.get(1).unwrap().as_str();
                let curr_ver: DeltaDataTypeVersion = curr_ver_str.parse().unwrap();
                if curr_ver > version {
                    // skip checkpoints newer than max version
                    continue;
                }
                if cp.is_none() || curr_ver > cp.unwrap().version {
                    let parts_str = captures.get(2).unwrap().as_str();
                    let parts = parts_str.parse().unwrap();
                    cp = Some(CheckPoint {
                        version: curr_ver,
                        size: 0,
                        parts: Some(parts),
                    });
                }
                continue;
            }
        }

        Ok(cp)
    }

    fn apply_log_from_bufread<R: BufRead>(
        &mut self,
        reader: BufReader<R>,
    ) -> Result<(), ApplyLogError> {
        for line in reader.lines() {
            let action: Action = serde_json::from_str(line?.as_str())?;
            process_action(&mut self.state, &action)?;
        }

        Ok(())
    }

    async fn apply_log(&mut self, version: DeltaDataTypeVersion) -> Result<(), ApplyLogError> {
        let log_path = self.version_to_log_path(version);
        let commit_log_bytes = self.storage.get_obj(&log_path).await?;
        let reader = BufReader::new(Cursor::new(commit_log_bytes));

        self.apply_log_from_bufread(reader)
    }

    async fn restore_checkpoint(&mut self, check_point: CheckPoint) -> Result<(), DeltaTableError> {
        let checkpoint_data_paths = self.get_checkpoint_data_paths(&check_point);
        // process actions from checkpoint
        self.state = DeltaTableState::default();
        for f in &checkpoint_data_paths {
            let obj = self.storage.get_obj(&f).await?;
            let preader = SerializedFileReader::new(SliceableCursor::new(obj))?;
            let schema = preader.metadata().file_metadata().schema();
            if !schema.is_group() {
                return Err(DeltaTableError::from(action::ActionError::Generic(
                    "Action record in checkpoint should be a struct".to_string(),
                )));
            }
            for record in preader.get_row_iter(None)? {
                process_action(
                    &mut self.state,
                    &Action::from_parquet_record(&schema, &record)?,
                )?;
            }
        }

        Ok(())
    }

    async fn get_latest_version(&mut self) -> Result<DeltaDataTypeVersion, DeltaTableError> {
        let mut version = match self.get_last_checkpoint().await {
            Ok(last_check_point) => last_check_point.version,
            Err(LoadCheckpointError::NotFound) => {
                // no checkpoint, start with version 0
                0
            }
            Err(e) => {
                return Err(DeltaTableError::LoadCheckpoint { source: e });
            }
        };

        // scan logs after checkpoint
        loop {
            match self
                .storage
                .head_obj(&self.version_to_log_path(version))
                .await
            {
                Ok(meta) => {
                    // also cache timestamp for version
                    self.version_timestamp
                        .insert(version, meta.modified.timestamp());
                    version += 1;
                }
                Err(e) => {
                    match e {
                        StorageError::NotFound => {
                            version -= 1;
                        }
                        _ => return Err(DeltaTableError::from(e)),
                    }
                    break;
                }
            }
        }

        Ok(version)
    }

    /// Load DeltaTable with data from latest checkpoint
    pub async fn load(&mut self) -> Result<(), DeltaTableError> {
        match self.get_last_checkpoint().await {
            Ok(last_check_point) => {
                self.last_check_point = Some(last_check_point);
                self.restore_checkpoint(last_check_point).await?;
                self.version = last_check_point.version + 1;
            }
            Err(LoadCheckpointError::NotFound) => {
                // no checkpoint, start with version 0
                self.version = 0;
            }
            Err(e) => {
                return Err(DeltaTableError::LoadCheckpoint { source: e });
            }
        }

        self.apply_logs_after_current_version().await?;

        Ok(())
    }

    /// Updates the DeltaTable to the most recent state committed to the transaction log.
    pub async fn update(&mut self) -> Result<(), DeltaTableError> {
        match self.get_last_checkpoint().await {
            Ok(last_check_point) => {
                if self.last_check_point != Some(last_check_point) {
                    self.last_check_point = Some(last_check_point);
                    self.restore_checkpoint(last_check_point).await?;
                    self.version = last_check_point.version + 1;
                }
            }
            Err(LoadCheckpointError::NotFound) => {
                self.version += 1;
            }
            Err(e) => {
                return Err(DeltaTableError::LoadCheckpoint { source: e });
            }
        }

        self.apply_logs_after_current_version().await?;

        Ok(())
    }

    async fn apply_logs_after_current_version(&mut self) -> Result<(), DeltaTableError> {
        // replay logs after checkpoint
        loop {
            match self.apply_log(self.version).await {
                Ok(_) => {
                    self.version += 1;
                }
                Err(e) => {
                    match e {
                        ApplyLogError::EndOfLog => {
                            self.version -= 1;
                            if self.version == -1 {
                                // no snapshot found, no 0 version found.  this is not a delta
                                // table, possibly an empty directroy.
                                return Err(DeltaTableError::NotATable);
                            }
                        }
                        _ => {
                            return Err(DeltaTableError::from(e));
                        }
                    }
                    break;
                }
            }
        }

        Ok(())
    }

    /// Loads the DeltaTable state for the given version.
    pub async fn load_version(
        &mut self,
        version: DeltaDataTypeVersion,
    ) -> Result<(), DeltaTableError> {
        // check if version is valid
        let log_path = self.version_to_log_path(version);
        match self.storage.head_obj(&log_path).await {
            Ok(_) => {}
            Err(StorageError::NotFound) => {
                return Err(DeltaTableError::InvalidVersion(version));
            }
            Err(e) => {
                return Err(DeltaTableError::from(e));
            }
        }
        self.version = version;

        let mut next_version;
        // 1. find latest checkpoint below version
        match self.find_latest_check_point_for_version(version).await? {
            Some(check_point) => {
                self.restore_checkpoint(check_point).await?;
                next_version = check_point.version + 1;
            }
            None => {
                // no checkpoint found, start from the beginning
                next_version = 0;
            }
        }

        // 2. apply all logs starting from checkpoint
        while next_version <= self.version {
            self.apply_log(next_version).await?;
            next_version += 1;
        }

        Ok(())
    }

    async fn get_version_timestamp(
        &mut self,
        version: DeltaDataTypeVersion,
    ) -> Result<i64, DeltaTableError> {
        match self.version_timestamp.get(&version) {
            Some(ts) => Ok(*ts),
            None => {
                let meta = self
                    .storage
                    .head_obj(&self.version_to_log_path(version))
                    .await?;
                let ts = meta.modified.timestamp();
                // also cache timestamp for version
                self.version_timestamp.insert(version, ts);

                Ok(ts)
            }
        }
    }

    /// Returns the file list tracked in current table state filtered by provided
    /// `PartitionFilter`s.
    pub fn get_files_by_partitions(
        &self,
        filters: &[PartitionFilter<&str>],
    ) -> Result<Vec<String>, DeltaTableError> {
        let partitions_number = match &self
            .state
            .current_metadata
            .as_ref()
            .ok_or(DeltaTableError::NoMetadata)?
            .partition_columns
        {
            partitions if !partitions.is_empty() => partitions.len(),
            _ => return Err(DeltaTableError::LoadPartitions),
        };
        let separator = "/";
        let files = self
            .state
            .files
            .iter()
            .filter(|add| {
                let partitions = add
                    .path
                    .splitn(partitions_number + 1, separator)
                    .filter_map(|p: &str| DeltaTablePartition::try_from(p).ok())
                    .collect::<Vec<DeltaTablePartition>>();
                filters
                    .iter()
                    .all(|filter| filter.match_partitions(&partitions))
            })
            .map(|add| add.path.clone())
            .collect();

        Ok(files)
    }

    /// Return the full file paths as strings for the partition(s)
    pub fn get_file_paths_by_partitions(
        &self,
        filters: &[PartitionFilter<&str>],
    ) -> Result<Vec<String>, DeltaTableError> {
        let files = self.get_files_by_partitions(filters)?;
        Ok(files
            .iter()
            .map(|fname| self.storage.join_path(&self.table_path, fname))
            .collect())
    }

    /// Return a refernece to the "add" actions present in the loaded state
    pub fn get_actions(&self) -> &Vec<action::Add> {
        &self.state.files
    }

    /// Returns an iterator of file names present in the loaded state
    #[inline]
    pub fn get_files_iter(&self) -> impl Iterator<Item = &str> {
        self.state.files.iter().map(|add| add.path.as_str())
    }

    /// Returns a collection of file names present in the loaded state
    #[inline]
    pub fn get_files(&self) -> Vec<&str> {
        self.get_files_iter().collect()
    }

    /// Returns a copy of the file paths present in the loaded state.
    pub fn get_file_paths(&self) -> Vec<String> {
        self.state
            .files
            .iter()
            .map(|add| self.storage.join_path(&self.table_path, &add.path))
            .collect()
    }

    /// Returns the metadata associated with the loaded state.
    pub fn get_metadata(&self) -> Result<&DeltaTableMetaData, DeltaTableError> {
        self.state
            .current_metadata
            .as_ref()
            .ok_or(DeltaTableError::NoMetadata)
    }

    /// Returns a vector of tombstones (i.e. `Remove` actions present in the current delta log.
    pub fn get_tombstones(&self) -> &Vec<action::Remove> {
        &self.state.tombstones
    }

    /// Returns the current version of the DeltaTable based on the loaded metadata.
    pub fn get_app_transaction_version(&self) -> &HashMap<String, DeltaDataTypeVersion> {
        &self.state.app_transaction_version
    }

    /// Returns the minimum reader version supported by the DeltaTable based on the loaded
    /// metadata.
    pub fn get_min_reader_version(&self) -> i32 {
        self.state.min_reader_version
    }

    /// Returns the minimum writer version supported by the DeltaTable based on the loaded
    /// metadata.
    pub fn get_min_writer_version(&self) -> i32 {
        self.state.min_writer_version
    }

    /// List files no longer referenced by a Delta table and are older than the retention threshold.
    fn get_stale_files(&self, retention_hours: u64) -> Result<Vec<String>, DeltaTableError> {
        if retention_hours < 168 {
            return Err(DeltaTableError::InvalidVacuumRetentionPeriod);
        }
        let before_duration = (SystemTime::now() - Duration::from_secs(3600 * retention_hours))
            .duration_since(UNIX_EPOCH);
        let delete_before_timestamp = match before_duration {
            Ok(duration) => duration.as_millis() as i64,
            Err(_) => return Err(DeltaTableError::InvalidVacuumRetentionPeriod),
        };

        Ok(self
            .get_tombstones()
            .iter()
            .filter(|tombstone| tombstone.deletionTimestamp < delete_before_timestamp)
            .map(|tombstone| self.storage.join_path(&self.table_path, &tombstone.path))
            .collect::<Vec<String>>())
    }

    /// Whether a path should be hidden for delta-related file operations, such as Vacuum.
    /// Names of the form partitionCol=[value] are partition directories, and should be
    /// deleted even if they'd normally be hidden. The _db_index directory contains (bloom filter)
    /// indexes and these must be deleted when the data they are tied to is deleted.
    fn is_hidden_directory(&self, path_name: &str) -> Result<bool, DeltaTableError> {
        Ok(
            (path_name.starts_with(&self.storage.join_path(&self.table_path, "."))
                || path_name.starts_with(&self.storage.join_path(&self.table_path, "_")))
                && !path_name
                    .starts_with(&self.storage.join_path(&self.table_path, "_delta_index"))
                && !path_name
                    .starts_with(&self.storage.join_path(&self.table_path, "_change_data"))
                && !self
                    .state
                    .current_metadata
                    .as_ref()
                    .ok_or(DeltaTableError::NoMetadata)?
                    .partition_columns
                    .iter()
                    .any(|partition_column| {
                        path_name.starts_with(
                            &self.storage.join_path(&self.table_path, partition_column),
                        )
                    }),
        )
    }

    /// Run the Vacuum command on the Delta Table: delete files no longer referenced by a Delta table and are older than the retention threshold.
    /// We do not recommend that you set a retention interval shorter than 7 days, because old snapshots and uncommitted files can still be in use by concurrent readers or writers to the table. If vacuum cleans up active files, concurrent readers can fail or, worse, tables can be corrupted when vacuum deletes files that have not yet been committed.
    pub async fn vacuum(
        &mut self,
        retention_hours: u64,
        dry_run: bool,
    ) -> Result<Vec<String>, DeltaTableError> {
        let tombstones_path = self.get_stale_files(retention_hours)?;

        let mut tombstones = vec![];
        let mut all_files = self.storage.list_objs(&self.table_path).await?;
        while let Some(obj_meta) = all_files.next().await {
            let obj_meta = obj_meta?;
            let is_not_valid_file = !self.get_file_paths().contains(&obj_meta.path);
            let is_valid_tombstone = tombstones_path.contains(&obj_meta.path);
            let is_not_hidden_directory = !self.is_hidden_directory(&obj_meta.path)?;
            if is_not_valid_file && is_valid_tombstone && is_not_hidden_directory {
                tombstones.push(obj_meta.path);
            }
        }

        if dry_run {
            return Ok(tombstones);
        }

        for tombstone in &tombstones {
            match self.storage.delete_obj(&tombstone).await {
                Ok(_) => continue,
                Err(StorageError::NotFound) => continue,
                Err(err) => return Err(DeltaTableError::StorageError { source: err }),
            }
        }

        Ok(tombstones)
    }

    /// Return table schema parsed from transaction log. Return None if table hasn't been loaded or
    /// no metadata was found in the log.
    pub fn schema(&self) -> Option<&Schema> {
        self.state.current_metadata.as_ref().map(|m| &m.schema)
    }

    /// Return table schema parsed from transaction log. Return `DeltaTableError` if table hasn't
    /// been loaded or no metadata was found in the log.
    pub fn get_schema(&self) -> Result<&Schema, DeltaTableError> {
        self.schema().ok_or(DeltaTableError::NoSchema)
    }

    /// Creates a new DeltaTransaction for the DeltaTable.
    /// The transaction holds a mutable reference to the DeltaTable, preventing other references
    /// until the transaction is dropped.
    pub fn create_transaction(
        &mut self,
        options: Option<DeltaTransactionOptions>,
    ) -> DeltaTransaction {
        DeltaTransaction::new(self, options)
    }

    /// Create a new Delta Table struct without loading any data from backing storage.
    ///
    /// NOTE: This is for advanced users. If you don't know why you need to use this method, please
    /// call one of the `open_table` helper methods instead.
    pub fn new(
        table_path: &str,
        storage_backend: Box<dyn StorageBackend>,
    ) -> Result<Self, DeltaTableError> {
        let log_path_normalized = storage_backend.join_path(table_path, "_delta_log");
        Ok(Self {
            version: 0,
            state: DeltaTableState::default(),
            storage: storage_backend,
            table_path: table_path.to_string(),
            last_check_point: None,
            log_path: log_path_normalized,
            version_timestamp: HashMap::new(),
        })
    }

    /// Time travel Delta table to latest version that's created at or before provided `datetime`
    /// argument.
    ///
    /// Internally, this methods performs a binary search on all Delta transaction logs.
    pub async fn load_with_datetime(
        &mut self,
        datetime: DateTime<Utc>,
    ) -> Result<(), DeltaTableError> {
        let mut min_version = 0;
        let mut max_version = self.get_latest_version().await?;
        let mut version = min_version;
        let target_ts = datetime.timestamp();

        // binary search
        while min_version <= max_version {
            let pivot = (max_version + min_version) / 2;
            version = pivot;
            let pts = self.get_version_timestamp(pivot).await?;

            match pts.cmp(&target_ts) {
                Ordering::Equal => {
                    break;
                }
                Ordering::Less => {
                    min_version = pivot + 1;
                }
                Ordering::Greater => {
                    max_version = pivot - 1;
                    version = max_version
                }
            }
        }

        if version < 0 {
            version = 0;
        }

        self.load_version(version).await
    }
}

impl fmt::Display for DeltaTable {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        writeln!(f, "DeltaTable({})", self.table_path)?;
        writeln!(f, "\tversion: {}", self.version)?;
        match self.state.current_metadata.as_ref() {
            Some(metadata) => {
                writeln!(f, "\tmetadata: {}", metadata)?;
            }
            None => {
                writeln!(f, "\tmetadata: None")?;
            }
        }
        writeln!(
            f,
            "\tmin_version: read={}, write={}",
            self.state.min_reader_version, self.state.min_writer_version
        )?;
        writeln!(f, "\tfiles count: {}", self.state.files.len())
    }
}

impl std::fmt::Debug for DeltaTable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> Result<(), std::fmt::Error> {
        write!(f, "DeltaTable <{}>", self.table_path)
    }
}

/// Error returned by the DeltaTransaction struct
#[derive(thiserror::Error, Debug)]
pub enum DeltaTransactionError {
    /// Error that indicates the transaction commit attempt failed. The wrapped inner error
    /// contains details.
    #[error("Transaction commit attempt failed. Last error: {inner}")]
    TransactionCommitAttempt {
        /// The wrapped TransactionCommitAttemptError.
        inner: TransactionCommitAttemptError,
    },

    /// Error that indicates a Delta version conflict. i.e. a writer tried to write version _N_ but
    /// version _N_ already exists in the delta log.
    #[error("Version already existed when writing transaction. Last error: {inner}")]
    VersionAlreadyExists {
        /// The wrapped TransactionCommitAttemptError.
        inner: TransactionCommitAttemptError,
    },

    /// Error that indicates the record batch is missing a partition column required by the Delta
    /// schema.
    #[error("RecordBatch is missing partition column in Delta schema.")]
    MissingPartitionColumn,

    /// Error that indicates the transaction failed due to an underlying storage error.
    /// Specific details of the error are described by the wrapped storage error.
    #[error("Storage interaction failed: {source}")]
    Storage {
        /// The wrapped StorageError.
        source: StorageError,
    },

    /// Error that wraps an underlying DeltaTable error.
    /// The wrapped error describes the specific cause.
    #[error("DeltaTable interaction failed: {source}")]
    DeltaTable {
        /// The wrapped DeltaTable error.
        #[from]
        source: DeltaTableError,
    },

    /// Error caused by a problem while using serde_json to serialize an action.
    #[error("Action serialization failed: {source}")]
    ActionSerializationFailed {
        /// The wrapped serde_json Error.
        #[from]
        source: serde_json::Error,
    },
}

/// Error that occurs when a single transaction commit attempt fails
#[derive(thiserror::Error, Debug)]
pub enum TransactionCommitAttemptError {
    // NOTE: it would be nice to add a `num_retries` prop to this error so we can identify how frequently we hit optimistic concurrency retries and look for optimization paths
    /// Error indicating the transaction commit attempt failed because the Delta table version has already been committed.
    /// This is expected in the case of multiple writers to the same table and retried within the
    /// optimistic concurrency loop.
    #[error("Version already exists: {source}")]
    VersionExists {
        /// The wrapped StorageError.
        source: StorageError,
    },

    /// Error indicating a general DeltaTable error occurred during a transaction commit attempt.
    #[error("Commit Failed due to DeltaTable error: {source}")]
    DeltaTable {
        /// The wrapped DeltaTableError
        #[from]
        source: DeltaTableError,
    },

    /// Error indicating a general StorageError occurred during a transaction commit attempt.
    #[error("Commit Failed due to StorageError: {source}")]
    Storage {
        /// The wrapped StorageError
        source: StorageError,
    },
}

impl From<TransactionCommitAttemptError> for DeltaTransactionError {
    fn from(error: TransactionCommitAttemptError) -> Self {
        match error {
            TransactionCommitAttemptError::VersionExists { .. } => {
                DeltaTransactionError::VersionAlreadyExists { inner: error }
            }
            _ => DeltaTransactionError::TransactionCommitAttempt { inner: error },
        }
    }
}
impl From<StorageError> for TransactionCommitAttemptError {
    fn from(error: StorageError) -> Self {
        match error {
            StorageError::AlreadyExists(_) => {
                TransactionCommitAttemptError::VersionExists { source: error }
            }
            _ => TransactionCommitAttemptError::Storage { source: error },
        }
    }
}

const DEFAULT_DELTA_MAX_RETRY_COMMIT_ATTEMPTS: u32 = 10_000_000;

/// Options for customizing behavior of a `DeltaTransaction`
#[derive(Debug)]
pub struct DeltaTransactionOptions {
    /// number of retry attempts allowed when committing a transaction
    max_retry_commit_attempts: u32,
}

impl DeltaTransactionOptions {
    /// Creates a new `DeltaTransactionOptions`
    pub fn new(max_retry_commit_attempts: u32) -> Self {
        Self {
            max_retry_commit_attempts,
        }
    }
}

impl Default for DeltaTransactionOptions {
    fn default() -> Self {
        Self {
            max_retry_commit_attempts: DEFAULT_DELTA_MAX_RETRY_COMMIT_ATTEMPTS,
        }
    }
}

/// Object representing a delta transaction.
/// Clients that do not need to mutate action content in case a transaction conflict is encountered
/// may use the `commit_with` method and rely on optimistic concurrency to determine the
/// appropriate Delta version number for a commit. A good example of this type of client is an
/// append only client that does not need to maintain transaction state with external systems.
/// Clients that may need to do conflict resolution if the Delta version changes should use the `commit_version`
/// method and manage the Delta version themselves so that they can resolve data conflicts that may
/// occur between Delta versions.
#[derive(Debug)]
pub struct DeltaTransaction<'a> {
    delta_table: &'a mut DeltaTable,
    options: DeltaTransactionOptions,
}

impl<'a> DeltaTransaction<'a> {
    /// Creates a new delta transaction.
    /// Holds a mutable reference to the delta table to prevent outside mutation while a transaction commit is in progress.
    /// Transaction behavior may be customized by passing an instance of `DeltaTransactionOptions`.
    pub fn new(delta_table: &'a mut DeltaTable, options: Option<DeltaTransactionOptions>) -> Self {
        DeltaTransaction {
            delta_table,
            options: options.unwrap_or_else(DeltaTransactionOptions::default),
        }
    }

    /// Commits the given actions to the delta log.
    /// This method will retry the transaction commit based on the value of `max_retry_commit_attempts` set in `DeltaTransactionOptions`.
    pub async fn commit_with(
        &mut self,
        additional_actions: &[Action],
        _operation: Option<DeltaOperation>,
    ) -> Result<DeltaDataTypeVersion, DeltaTransactionError> {
        // TODO: stubbing `operation` parameter (which will be necessary for writing the CommitInfo action), but leaving it unused for now.
        // `CommitInfo` is a fairly dynamic data structure so we should work out the data structure approach separately.

        // TODO: calculate isolation level to use when checking for conflicts.
        // Leaving conflict checking unimplemented for now to get the "single writer" implementation off the ground.
        // Leaving some commmented code in place as a guidepost for the future.

        // let no_data_changed = actions.iter().all(|a| match a {
        //     Action::add(x) => !x.dataChange,
        //     Action::remove(x) => !x.dataChange,
        //     _ => false,
        // });
        // let isolation_level = if no_data_changed {
        //     IsolationLevel::SnapshotIsolation
        // } else {
        //     IsolationLevel::Serializable
        // };

        // TODO: create a CommitInfo action and prepend it to actions.

        // Serialize all actions that are part of this log entry.
        let log_entry = log_entry_from_actions(additional_actions)?;

        // try to commit in a loop in case other writers write the next version first
        let version = self.try_commit_loop(log_entry.as_bytes()).await?;

        // NOTE: since we have the log entry in memory already,
        // we could optimize this further by merging the log entry instead of updating from storage.
        self.delta_table.update().await?;

        Ok(version)
    }

    /// Commits the delta transaction at the specified version.
    /// Propagates version conflict errors back to the caller immediately.
    pub async fn commit_version(
        &mut self,
        version: DeltaDataTypeVersion,
        additional_actions: &[Action],
        _operation: Option<DeltaOperation>,
    ) -> Result<DeltaDataTypeVersion, DeltaTransactionError> {
        // TODO: create a CommitInfo action and prepend it to actions.

        let log_entry = log_entry_from_actions(additional_actions)?;
        let tmp_log_path = self.prepare_commit(log_entry.as_bytes()).await?;
        let version = self.try_commit(&tmp_log_path, version).await?;

        self.delta_table.update().await?;

        Ok(version)
    }

    async fn try_commit_loop(
        &mut self,
        log_entry: &[u8],
    ) -> Result<DeltaDataTypeVersion, TransactionCommitAttemptError> {
        let mut attempt_number: u32 = 0;

        let tmp_log_path = self.prepare_commit(log_entry).await?;
        loop {
            let version = self.next_attempt_version().await?;

            let commit_result = self.try_commit(&tmp_log_path, version).await;

            match commit_result {
                Ok(v) => {
                    return Ok(v);
                }
                Err(e) => {
                    match e {
                        TransactionCommitAttemptError::VersionExists { .. }
                            if attempt_number > self.options.max_retry_commit_attempts + 1 =>
                        {
                            debug!("Transaction attempt failed. Attempts exhausted beyond max_retry_commit_attempts of {} so failing.", self.options.max_retry_commit_attempts);
                            return Err(e);
                        }
                        TransactionCommitAttemptError::VersionExists { .. } => {
                            attempt_number += 1;
                            debug!("Transaction attempt failed. Incrementing attempt number to {} and retrying.", attempt_number);
                        }
                        // NOTE: Add other retryable errors as needed here
                        _ => {
                            return Err(e);
                        }
                    }
                }
            }
        }
    }

    async fn prepare_commit(
        &mut self,
        log_entry: &[u8],
    ) -> Result<String, TransactionCommitAttemptError> {
        let token = Uuid::new_v4().to_string();
        let tmp_log_path = self.delta_table.tmp_commit_log_path(&token);

        self.delta_table
            .storage
            .put_obj(&tmp_log_path, log_entry)
            .await?;

        Ok(tmp_log_path)
    }

    async fn try_commit(
        &mut self,
        tmp_log_path: &str,
        version: DeltaDataTypeVersion,
    ) -> Result<DeltaDataTypeVersion, TransactionCommitAttemptError> {
        let log_path = self.delta_table.version_to_log_path(version);

        // move temporary commit file to delta log directory
        // rely on storage to fail if the file already exists -
        self.delta_table
            .storage
            .rename_obj(tmp_log_path, &log_path)
            .await?;

        Ok(version)
    }

    async fn next_attempt_version(
        &mut self,
    ) -> Result<DeltaDataTypeVersion, TransactionCommitAttemptError> {
        self.delta_table.update().await?;
        Ok(self.delta_table.version + 1)
    }
}

fn log_entry_from_actions(actions: &[Action]) -> Result<String, serde_json::Error> {
    let mut jsons = Vec::<String>::new();

    for action in actions {
        let json = serde_json::to_string(action)?;
        jsons.push(json);
    }

    Ok(jsons.join("\n"))
}

fn process_action(
    state: &mut DeltaTableState,
    action: &Action,
) -> Result<(), serde_json::error::Error> {
    match action {
        Action::add(v) => {
            state.files.push(v.clone());
        }
        Action::remove(v) => {
            state.files.retain(|a| *a.path != v.path);
            state.tombstones.push(v.clone());
        }
        Action::protocol(v) => {
            state.min_reader_version = v.minReaderVersion;
            state.min_writer_version = v.minWriterVersion;
        }
        Action::metaData(v) => {
            state.current_metadata = Some(DeltaTableMetaData {
                id: v.id.clone(),
                name: v.name.clone(),
                description: v.description.clone(),
                format: v.format.clone(),
                schema: v.get_schema()?,
                partition_columns: v.partitionColumns.clone(),
                created_time: v.createdTime,
                configuration: v.configuration.clone(),
            });
        }
        Action::txn(v) => {
            *state
                .app_transaction_version
                .entry(v.appId.clone())
                .or_insert(v.version) = v.version;
        }
        Action::commitInfo(v) => {
            state.commit_infos.push(v.clone());
        }
    }

    Ok(())
}

/// Creates and loads a DeltaTable from the given path with current metadata.
/// Infers the storage backend to use from the scheme in the given table path.
pub async fn open_table(table_path: &str) -> Result<DeltaTable, DeltaTableError> {
    let storage_backend = storage::get_backend_for_uri(table_path)?;
    let mut table = DeltaTable::new(table_path, storage_backend)?;
    table.load().await?;

    Ok(table)
}

/// Creates a DeltaTable from the given path and loads it with the metadata from the given version.
/// Infers the storage backend to use from the scheme in the given table path.
pub async fn open_table_with_version(
    table_path: &str,
    version: DeltaDataTypeVersion,
) -> Result<DeltaTable, DeltaTableError> {
    let storage_backend = storage::get_backend_for_uri(table_path)?;
    let mut table = DeltaTable::new(table_path, storage_backend)?;
    table.load_version(version).await?;

    Ok(table)
}

/// Creates a DeltaTable from the given path.
/// Loads metadata from the version appropriate based on the given ISO-8601/RFC-3339 timestamp.
/// Infers the storage backend to use from the scheme in the given table path.
pub async fn open_table_with_ds(table_path: &str, ds: &str) -> Result<DeltaTable, DeltaTableError> {
    let datetime = DateTime::<Utc>::from(DateTime::<FixedOffset>::parse_from_rfc3339(ds)?);
    let storage_backend = storage::get_backend_for_uri(table_path)?;
    let mut table = DeltaTable::new(table_path, storage_backend)?;
    table.load_with_datetime(datetime).await?;

    Ok(table)
}

/// Returns rust create version, can be use used in language bindings to expose Rust core version
pub fn crate_version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

#[cfg(test)]
mod tests {
    use super::action;
    use super::action::Action;
    use super::{process_action, DeltaTableState};
    use std::collections::HashMap;

    #[test]
    fn state_records_new_txn_version() {
        let mut app_transaction_version = HashMap::new();
        app_transaction_version.insert("abc".to_string(), 1);
        app_transaction_version.insert("xyz".to_string(), 1);

        let mut state = DeltaTableState {
            files: vec![],
            commit_infos: vec![],
            tombstones: vec![],
            current_metadata: None,
            min_reader_version: 1,
            min_writer_version: 2,
            app_transaction_version,
        };

        let txn_action = Action::txn(action::Txn {
            appId: "abc".to_string(),
            version: 2,
            lastUpdated: 0,
        });

        let _ = process_action(&mut state, &txn_action).unwrap();

        assert_eq!(2, *state.app_transaction_version.get("abc").unwrap());
        assert_eq!(1, *state.app_transaction_version.get("xyz").unwrap());
    }
}
