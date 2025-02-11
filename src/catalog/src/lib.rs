// Copyright Materialize, Inc. and contributors. All rights reserved.
//
// Use of this software is governed by the Business Source License
// included in the LICENSE file.
//
// As of the Change Date specified in that file, in accordance with
// the Business Source License, use of this software will be governed
// by the Apache License, Version 2.0.

// BEGIN LINT CONFIG
// DO NOT EDIT. Automatically generated by bin/gen-lints.
// Have complaints about the noise? See the note in misc/python/materialize/cli/gen-lints.py first.
#![allow(unknown_lints)]
#![allow(clippy::style)]
#![allow(clippy::complexity)]
#![allow(clippy::large_enum_variant)]
#![allow(clippy::mutable_key_type)]
#![allow(clippy::stable_sort_primitive)]
#![allow(clippy::map_entry)]
#![allow(clippy::box_default)]
#![allow(clippy::drain_collect)]
#![warn(clippy::bool_comparison)]
#![warn(clippy::clone_on_ref_ptr)]
#![warn(clippy::no_effect)]
#![warn(clippy::unnecessary_unwrap)]
#![warn(clippy::dbg_macro)]
#![warn(clippy::todo)]
#![warn(clippy::wildcard_dependencies)]
#![warn(clippy::zero_prefixed_literal)]
#![warn(clippy::borrowed_box)]
#![warn(clippy::deref_addrof)]
#![warn(clippy::double_must_use)]
#![warn(clippy::double_parens)]
#![warn(clippy::extra_unused_lifetimes)]
#![warn(clippy::needless_borrow)]
#![warn(clippy::needless_question_mark)]
#![warn(clippy::needless_return)]
#![warn(clippy::redundant_pattern)]
#![warn(clippy::redundant_slicing)]
#![warn(clippy::redundant_static_lifetimes)]
#![warn(clippy::single_component_path_imports)]
#![warn(clippy::unnecessary_cast)]
#![warn(clippy::useless_asref)]
#![warn(clippy::useless_conversion)]
#![warn(clippy::builtin_type_shadow)]
#![warn(clippy::duplicate_underscore_argument)]
#![warn(clippy::double_neg)]
#![warn(clippy::unnecessary_mut_passed)]
#![warn(clippy::wildcard_in_or_patterns)]
#![warn(clippy::crosspointer_transmute)]
#![warn(clippy::excessive_precision)]
#![warn(clippy::overflow_check_conditional)]
#![warn(clippy::as_conversions)]
#![warn(clippy::match_overlapping_arm)]
#![warn(clippy::zero_divided_by_zero)]
#![warn(clippy::must_use_unit)]
#![warn(clippy::suspicious_assignment_formatting)]
#![warn(clippy::suspicious_else_formatting)]
#![warn(clippy::suspicious_unary_op_formatting)]
#![warn(clippy::mut_mutex_lock)]
#![warn(clippy::print_literal)]
#![warn(clippy::same_item_push)]
#![warn(clippy::useless_format)]
#![warn(clippy::write_literal)]
#![warn(clippy::redundant_closure)]
#![warn(clippy::redundant_closure_call)]
#![warn(clippy::unnecessary_lazy_evaluations)]
#![warn(clippy::partialeq_ne_impl)]
#![warn(clippy::redundant_field_names)]
#![warn(clippy::transmutes_expressible_as_ptr_casts)]
#![warn(clippy::unused_async)]
#![warn(clippy::disallowed_methods)]
#![warn(clippy::disallowed_macros)]
#![warn(clippy::disallowed_types)]
#![warn(clippy::from_over_into)]
// END LINT CONFIG
// Disallow usage of `unwrap()`.
#![warn(clippy::unwrap_used)]

//! This crate is responsible for durable storing and modifying the catalog contents.

use async_trait::async_trait;
use std::collections::BTreeMap;
use std::fmt;
use std::fmt::{Debug, Formatter};
use std::num::NonZeroI64;
use std::time::Duration;

use mz_proto::TryFromProtoError;
use mz_sql::catalog::{
    CatalogError as SqlCatalogError, DefaultPrivilegeAclItem, DefaultPrivilegeObject,
};
use mz_stash::StashError;

pub use crate::objects::{
    Cluster, ClusterConfig, ClusterReplica, ClusterVariant, ClusterVariantManaged, Database, Item,
    ReplicaConfig, ReplicaLocation, Role, Schema, SystemObjectMapping,
};
pub use crate::stash::{Connection, ALL_COLLECTIONS};
pub use crate::transaction::Transaction;
use crate::transaction::TransactionBatch;
pub use initialize::{
    AUDIT_LOG_COLLECTION, CLUSTER_COLLECTION, CLUSTER_INTROSPECTION_SOURCE_INDEX_COLLECTION,
    CLUSTER_REPLICA_COLLECTION, COMMENTS_COLLECTION, CONFIG_COLLECTION, DATABASES_COLLECTION,
    DEFAULT_PRIVILEGES_COLLECTION, ID_ALLOCATOR_COLLECTION, ITEM_COLLECTION, ROLES_COLLECTION,
    SCHEMAS_COLLECTION, SETTING_COLLECTION, STORAGE_USAGE_COLLECTION,
    SYSTEM_CONFIGURATION_COLLECTION, SYSTEM_GID_MAPPING_COLLECTION, SYSTEM_PRIVILEGES_COLLECTION,
    TIMESTAMP_COLLECTION,
};
use mz_audit_log::{VersionedEvent, VersionedStorageUsage};
use mz_controller_types::{ClusterId, ReplicaId};
use mz_ore::collections::CollectionExt;
use mz_repr::adt::mz_acl_item::MzAclItem;
use mz_repr::role_id::RoleId;
use mz_repr::GlobalId;
use mz_sql::names::CommentObjectId;
use mz_storage_types::sources::Timeline;

mod stash;
mod transaction;

pub mod builtin;
pub mod initialize;
pub mod objects;

const DATABASE_ID_ALLOC_KEY: &str = "database";
const SCHEMA_ID_ALLOC_KEY: &str = "schema";
const USER_ROLE_ID_ALLOC_KEY: &str = "user_role";
const USER_CLUSTER_ID_ALLOC_KEY: &str = "user_compute";
const SYSTEM_CLUSTER_ID_ALLOC_KEY: &str = "system_compute";
const USER_REPLICA_ID_ALLOC_KEY: &str = "replica";
const SYSTEM_REPLICA_ID_ALLOC_KEY: &str = "system_replica";
pub const AUDIT_LOG_ID_ALLOC_KEY: &str = "auditlog";
pub const STORAGE_USAGE_ID_ALLOC_KEY: &str = "storage_usage";

#[derive(Debug)]
pub enum Error {
    Catalog(SqlCatalogError),
    // TODO(jkosh44) make this more generic.
    Stash(StashError),
}

impl std::error::Error for Error {}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Error::Catalog(e) => write!(f, "{e}"),
            Error::Stash(e) => write!(f, "{e}"),
        }
    }
}

impl From<SqlCatalogError> for Error {
    fn from(e: SqlCatalogError) -> Self {
        Self::Catalog(e)
    }
}

impl From<StashError> for Error {
    fn from(e: StashError) -> Self {
        Self::Stash(e)
    }
}

impl From<TryFromProtoError> for Error {
    fn from(e: TryFromProtoError) -> Error {
        Error::Stash(mz_stash::StashError::from(e))
    }
}

#[derive(Clone, Debug)]
pub struct BootstrapArgs {
    pub default_cluster_replica_size: String,
    pub builtin_cluster_replica_size: String,
    pub bootstrap_role: Option<String>,
}

/// A read only API for the durable catalog state.
#[async_trait]
pub trait ReadOnlyDurableCatalogState: Debug + Send {
    /// Reports if the catalog state has been initialized.
    async fn is_initialized(&mut self) -> Result<bool, Error>;

    // TODO(jkosh44) add and implement open methods to be implementation agnostic.
    /*   /// Checks to see if opening the catalog would be
    /// successful, without making any durable changes.
    ///
    /// Will return an error in the following scenarios:
    ///   - Catalog not initialized.
    ///   - Catalog migrations fail.
    async fn check_open(&self) -> Result<(), Error>;

    /// Opens the catalog in read only mode. All mutating methods
    /// will return an error.
    ///
    /// If the catalog is uninitialized or requires a migrations, then
    /// it will fail to open in read only mode.
    async fn open_read_only(&mut self) -> Result<(), Error>;*/

    /// Returns the epoch of the current durable catalog state. The epoch acts as
    /// a fencing token to prevent split brain issues across two
    /// [`DurableCatalogState`]s. When a new [`DurableCatalogState`] opens the
    /// catalog, it will increment the epoch by one (or initialize it to some
    /// value if there's no existing epoch) and store the value in memory. It's
    /// guaranteed that no two [`DurableCatalogState`]s will return the same value
    /// for their epoch.
    ///
    /// None is returned if the catalog hasn't been opened yet.
    ///
    /// NB: We may remove this in later iterations of Pv2.
    fn epoch(&mut self) -> Option<NonZeroI64>;

    /// Returns the version of Materialize that last wrote to the catalog.
    ///
    /// If the catalog is uninitialized this will return None.
    async fn get_catalog_content_version(&mut self) -> Result<Option<String>, Error>;

    /// Get all clusters.
    async fn get_clusters(&mut self) -> Result<Vec<Cluster>, Error>;

    /// Get all cluster replicas.
    async fn get_cluster_replicas(&mut self) -> Result<Vec<ClusterReplica>, Error>;

    /// Get all databases.
    async fn get_databases(&mut self) -> Result<Vec<Database>, Error>;

    /// Get all schemas.
    async fn get_schemas(&mut self) -> Result<Vec<Schema>, Error>;

    /// Get all system items.
    async fn get_system_items(&mut self) -> Result<Vec<SystemObjectMapping>, Error>;

    /// Get all introspection source indexes.
    ///
    /// Returns (index-name, global-id).
    async fn get_introspection_source_indexes(
        &mut self,
        cluster_id: ClusterId,
    ) -> Result<BTreeMap<String, GlobalId>, Error>;

    /// Get all roles.
    async fn get_roles(&mut self) -> Result<Vec<Role>, Error>;

    /// Get all default privileges.
    async fn get_default_privileges(
        &mut self,
    ) -> Result<Vec<(DefaultPrivilegeObject, DefaultPrivilegeAclItem)>, Error>;

    /// Get all system privileges.
    async fn get_system_privileges(&mut self) -> Result<Vec<MzAclItem>, Error>;

    /// Get all system configurations.
    async fn get_system_configurations(&mut self) -> Result<BTreeMap<String, String>, Error>;

    /// Get all comments.
    async fn get_comments(
        &mut self,
    ) -> Result<Vec<(CommentObjectId, Option<usize>, String)>, Error>;

    /// Get all timelines and their persisted timestamps.
    // TODO(jkosh44) This should be removed once the timestamp oracle is extracted.
    async fn get_timestamps(&mut self) -> Result<BTreeMap<Timeline, mz_repr::Timestamp>, Error>;

    /// Get the persisted timestamp of a timeline.
    // TODO(jkosh44) This should be removed once the timestamp oracle is extracted.
    async fn get_timestamp(
        &mut self,
        timeline: &Timeline,
    ) -> Result<Option<mz_repr::Timestamp>, Error>;

    /// Get all audit log events.
    async fn get_audit_logs(&mut self) -> Result<Vec<VersionedEvent>, Error>;

    /// Get the next ID of `id_type`, without allocating it.
    async fn get_next_id(&mut self, id_type: &str) -> Result<u64, Error>;

    /// Get the next system replica id without allocating it.
    async fn get_next_system_replica_id(&mut self) -> Result<u64, Error> {
        self.get_next_id(SYSTEM_REPLICA_ID_ALLOC_KEY).await
    }

    /// Get the next user replica id without allocating it.
    async fn get_next_user_replica_id(&mut self) -> Result<u64, Error> {
        self.get_next_id(USER_REPLICA_ID_ALLOC_KEY).await
    }

    // TODO(jkosh44) Implement this for the catalog debug tool.
    /*    /// Dumps the entire catalog contents in human readable JSON.
    async fn dump(&self) -> Result<String, Error>;*/
}

/// A read-write API for the durable catalog state.
#[async_trait]
pub trait DurableCatalogState: ReadOnlyDurableCatalogState {
    // TODO(jkosh44) add and implement open methods to be implementation agnostic.
    /*/// Opens the catalog in a writeable mode. Initializes the
    /// catalog, if it is uninitialized, and executes migrations.
    async fn open(&mut self) -> Result<(), Error>;*/

    /// Returns true if the catalog is opened in read only mode, false otherwise.
    fn is_read_only(&self) -> bool;

    /// Creates a new durable catalog state transaction.
    async fn transaction(&mut self) -> Result<Transaction, Error>;

    /// Commits a durable catalog state transaction.
    async fn commit_transaction(&mut self, txn_batch: TransactionBatch) -> Result<(), Error>;

    /// Confirms that this catalog is connected as the current leader.
    ///
    /// NB: We may remove this in later iterations of Pv2.
    async fn confirm_leadership(&mut self) -> Result<(), Error>;

    /// Set's the connection timeout for the underlying durable store.
    async fn set_connect_timeout(&mut self, connect_timeout: Duration);

    /// Persist the version of Materialize that last wrote to the catalog.
    async fn set_catalog_content_version(&mut self, new_version: &str) -> Result<(), Error>;

    /// Gets all storage usage events and permanently deletes from the catalog those
    /// that happened more than the retention period ago from boot_ts.
    async fn get_and_prune_storage_usage(
        &mut self,
        retention_period: Option<Duration>,
        boot_ts: mz_repr::Timestamp,
    ) -> Result<Vec<VersionedStorageUsage>, Error>;

    /// Persist system items.
    async fn set_system_items(&mut self, mappings: Vec<SystemObjectMapping>) -> Result<(), Error>;

    /// Persist introspection source indexes.
    ///
    /// `mappings` has the format (cluster-id, index-name, global-id).
    ///
    /// Panics if the provided id is not a system id.
    async fn set_introspection_source_indexes(
        &mut self,
        mappings: Vec<(ClusterId, &str, GlobalId)>,
    ) -> Result<(), Error>;

    /// Persist the configuration of a replica.
    /// This accepts only one item, as we currently use this only for the default cluster
    async fn set_replica_config(
        &mut self,
        replica_id: ReplicaId,
        cluster_id: ClusterId,
        name: String,
        config: ReplicaConfig,
        owner_id: RoleId,
    ) -> Result<(), Error>;

    /// Persist new global timestamp for a timeline.
    async fn set_timestamp(
        &mut self,
        timeline: &Timeline,
        timestamp: mz_repr::Timestamp,
    ) -> Result<(), Error>;

    /// Persist the deployment generation of this instance.
    async fn set_deploy_generation(&mut self, deploy_generation: u64) -> Result<(), Error>;

    /// Allocates and returns `amount` IDs of `id_type`.
    async fn allocate_id(&mut self, id_type: &str, amount: u64) -> Result<Vec<u64>, Error>;

    /// Allocates and returns `amount` system [`GlobalId`]s.
    async fn allocate_system_ids(&mut self, amount: u64) -> Result<Vec<GlobalId>, Error> {
        let id = self.allocate_id("system", amount).await?;

        Ok(id.into_iter().map(GlobalId::System).collect())
    }

    /// Allocates and returns a user [`GlobalId`].
    async fn allocate_user_id(&mut self) -> Result<GlobalId, Error> {
        let id = self.allocate_id("user", 1).await?;
        let id = id.into_element();
        Ok(GlobalId::User(id))
    }

    /// Allocates and returns a system [`ClusterId`].
    async fn allocate_system_cluster_id(&mut self) -> Result<ClusterId, Error> {
        let id = self.allocate_id(SYSTEM_CLUSTER_ID_ALLOC_KEY, 1).await?;
        let id = id.into_element();
        Ok(ClusterId::System(id))
    }

    /// Allocates and returns a user [`ClusterId`].
    async fn allocate_user_cluster_id(&mut self) -> Result<ClusterId, Error> {
        let id = self.allocate_id(USER_CLUSTER_ID_ALLOC_KEY, 1).await?;
        let id = id.into_element();
        Ok(ClusterId::User(id))
    }

    /// Allocates and returns a user [`ReplicaId`].
    async fn allocate_user_replica_id(&mut self) -> Result<ReplicaId, Error> {
        let id = self.allocate_id(USER_REPLICA_ID_ALLOC_KEY, 1).await?;
        let id = id.into_element();
        Ok(ReplicaId::User(id))
    }
}
