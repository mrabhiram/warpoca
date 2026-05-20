use crate::{
    auth::UserUid,
    cloud_object::{
        model::actions::{
            ObjectAction, ObjectActionHistory, ObjectActionSubtype, ObjectActionType,
        },
        model::generic_string_model::GenericStringObjectId,
        BulkCreateCloudObjectResult, BulkCreateGenericStringObjectsRequest,
        CreateCloudObjectResult, CreateObjectRequest, CreatedCloudObject,
        GenericStringObjectFormat, GenericStringObjectUniqueKey, ObjectDeleteResult, ObjectIdType,
        ObjectMetadataUpdateResult, ObjectPermissionUpdateResult, ObjectPermissionsUpdateData,
        ObjectType, ObjectsToUpdate, Owner, Revision, RevisionAndLastEditor, ServerFolder,
        ServerMetadata, ServerNotebook, ServerObject, ServerPermissions, ServerWorkflow,
        UpdateCloudObjectResult,
    },
    drive::{folders::FolderId, sharing::SharingAccessLevel},
    notebooks::NotebookId,
    server::{
        cloud_objects::{
            listener::ObjectUpdateMessage,
            update_manager::{GetCloudObjectResponse, InitialLoadResponse},
        },
        ids::{ClientId, ServerId, ServerIdAndType, SyncId},
        server_api::object::{GuestIdentifier, ObjectClient},
        sync_queue::SerializedModel,
    },
    workflows::WorkflowId,
};
use anyhow::{anyhow, Result};
use async_channel::Sender;
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use std::collections::HashMap;
use uuid::Uuid;
use warp_graphql::{object_permissions::AccessLevel, scalars::time::ServerTimestamp};

pub struct LocalObjectClient;

impl LocalObjectClient {
    pub fn new() -> Self {
        Self
    }
}

#[cfg_attr(not(target_family = "wasm"), async_trait)]
#[cfg_attr(target_family = "wasm", async_trait(?Send))]
impl ObjectClient for LocalObjectClient {
    async fn create_workflow(
        &self,
        request: CreateObjectRequest,
    ) -> Result<CreateCloudObjectResult> {
        Ok(created_object(request, ObjectIdType::Workflow))
    }

    async fn update_workflow(
        &self,
        _workflow_id: WorkflowId,
        _data: SerializedModel,
        _revision: Option<Revision>,
    ) -> Result<UpdateCloudObjectResult<ServerWorkflow>> {
        Ok(update_success())
    }

    async fn bulk_create_generic_string_objects(
        &self,
        owner: Owner,
        objects: &[BulkCreateGenericStringObjectsRequest],
    ) -> Result<BulkCreateCloudObjectResult> {
        let created_cloud_objects = objects
            .iter()
            .map(|object| created_cloud_object(object.id, ObjectIdType::GenericStringObject, owner))
            .collect();
        Ok(BulkCreateCloudObjectResult::Success {
            created_cloud_objects,
        })
    }

    async fn create_generic_string_object(
        &self,
        _format: GenericStringObjectFormat,
        _uniqueness_key: Option<GenericStringObjectUniqueKey>,
        request: CreateObjectRequest,
    ) -> Result<CreateCloudObjectResult> {
        Ok(created_object(request, ObjectIdType::GenericStringObject))
    }

    async fn create_notebook(
        &self,
        request: CreateObjectRequest,
    ) -> Result<CreateCloudObjectResult> {
        Ok(created_object(request, ObjectIdType::Notebook))
    }

    async fn update_notebook(
        &self,
        _notebook_id: NotebookId,
        _title: Option<String>,
        _data: Option<SerializedModel>,
        _revision: Option<Revision>,
    ) -> Result<UpdateCloudObjectResult<ServerNotebook>> {
        Ok(update_success())
    }

    async fn create_folder(&self, request: CreateObjectRequest) -> Result<CreateCloudObjectResult> {
        Ok(created_object(request, ObjectIdType::Folder))
    }

    async fn update_folder(
        &self,
        _folder_id: FolderId,
        _name: SerializedModel,
    ) -> Result<UpdateCloudObjectResult<ServerFolder>> {
        Ok(update_success())
    }

    async fn update_generic_string_object(
        &self,
        _object_id: GenericStringObjectId,
        _model: SerializedModel,
        _revision: Option<Revision>,
    ) -> Result<UpdateCloudObjectResult<Box<dyn ServerObject>>> {
        Ok(update_success())
    }

    async fn grab_notebook_edit_access(&self, notebook_id: NotebookId) -> Result<ServerMetadata> {
        let server_id: ServerId = notebook_id.into();
        Ok(server_metadata(server_id, None))
    }

    async fn give_up_notebook_edit_access(
        &self,
        notebook_id: NotebookId,
    ) -> Result<ServerMetadata> {
        let server_id: ServerId = notebook_id.into();
        Ok(server_metadata(server_id, None))
    }

    async fn get_warp_drive_updates(
        &self,
        _message_sender: Sender<ObjectUpdateMessage>,
        stream_ready_sender: Sender<()>,
    ) -> Result<()> {
        let _ = stream_ready_sender.send(()).await;
        futures::future::pending::<()>().await;
        Ok(())
    }

    async fn fetch_changed_objects(
        &self,
        _objects_to_update: ObjectsToUpdate,
        _force_refresh: bool,
    ) -> Result<InitialLoadResponse> {
        Ok(InitialLoadResponse::default())
    }

    async fn fetch_single_cloud_object(&self, _id: ServerId) -> Result<GetCloudObjectResponse> {
        Err(anyhow!(
            "WarpOCA local object store has no remote object fetch"
        ))
    }

    async fn transfer_notebook_owner(
        &self,
        _notebook_id: NotebookId,
        _owner: Owner,
    ) -> Result<bool> {
        Ok(true)
    }

    async fn transfer_workflow_owner(
        &self,
        _workflow_id: WorkflowId,
        _owner: Owner,
    ) -> Result<bool> {
        Ok(true)
    }

    async fn transfer_generic_string_object_owner(
        &self,
        _workflow_id: GenericStringObjectId,
        _owner: Owner,
    ) -> Result<bool> {
        Ok(true)
    }

    async fn trash_object(&self, _id: ServerId) -> Result<bool> {
        Ok(true)
    }

    async fn untrash_object(&self, id: ServerId) -> Result<ObjectMetadataUpdateResult> {
        Ok(ObjectMetadataUpdateResult::Success {
            metadata: Box::new(server_metadata(id, None)),
        })
    }

    async fn delete_object(&self, id: ServerId) -> Result<ObjectDeleteResult> {
        Ok(ObjectDeleteResult::Success {
            deleted_ids: vec![SyncId::ServerId(id)],
        })
    }

    async fn empty_trash(&self, _owner: Owner) -> Result<ObjectDeleteResult> {
        Ok(ObjectDeleteResult::Success {
            deleted_ids: Vec::new(),
        })
    }

    async fn move_object(
        &self,
        _id: ServerId,
        _folder_id: Option<FolderId>,
        _owner: Owner,
        _object_type: ObjectType,
    ) -> Result<bool> {
        Ok(true)
    }

    async fn record_object_action(
        &self,
        id: ServerId,
        action_type: ObjectActionType,
        timestamp: DateTime<Utc>,
        data: Option<String>,
    ) -> Result<ObjectActionHistory> {
        let hashed_sqlite_id = id.sqlite_type_and_uid_hash(ObjectIdType::Workflow);
        Ok(ObjectActionHistory {
            uid: id.uid(),
            hashed_sqlite_id: hashed_sqlite_id.clone(),
            latest_processed_at_timestamp: Utc::now(),
            actions: vec![ObjectAction {
                action_type,
                uid: id.uid(),
                hashed_sqlite_id,
                action_subtype: ObjectActionSubtype::SingleAction {
                    timestamp,
                    processed_at_timestamp: Some(Utc::now()),
                    data,
                    pending: false,
                },
            }],
        })
    }

    async fn leave_object(&self, id: ServerId) -> Result<ObjectDeleteResult> {
        self.delete_object(id).await
    }

    async fn set_object_link_permissions(
        &self,
        _object_id: ServerId,
        _access_level: SharingAccessLevel,
    ) -> Result<ObjectPermissionUpdateResult> {
        Ok(ObjectPermissionUpdateResult::Success)
    }

    async fn remove_object_link_permissions(
        &self,
        _object_id: ServerId,
    ) -> Result<ObjectPermissionUpdateResult> {
        Ok(ObjectPermissionUpdateResult::Success)
    }

    async fn add_object_guests(
        &self,
        _object_id: ServerId,
        _guest_emails: Vec<String>,
        _access_level: AccessLevel,
    ) -> Result<ObjectPermissionsUpdateData> {
        Ok(ObjectPermissionsUpdateData {
            permissions: personal_permissions(local_owner()),
            profiles: Vec::new(),
        })
    }

    async fn update_object_guests(
        &self,
        _object_id: ServerId,
        _guest_emails: Vec<String>,
        _access_level: AccessLevel,
    ) -> Result<ServerPermissions> {
        Ok(personal_permissions(local_owner()))
    }

    async fn remove_object_guest(
        &self,
        _object_id: ServerId,
        _guest: GuestIdentifier,
    ) -> Result<ServerPermissions> {
        Ok(personal_permissions(local_owner()))
    }

    async fn fetch_environment_last_task_run_timestamps(
        &self,
    ) -> Result<HashMap<String, DateTime<Utc>>> {
        Ok(HashMap::new())
    }
}

fn created_object(request: CreateObjectRequest, id_type: ObjectIdType) -> CreateCloudObjectResult {
    CreateCloudObjectResult::Success {
        created_cloud_object: created_cloud_object(request.client_id, id_type, request.owner),
    }
}

fn created_cloud_object(
    client_id: ClientId,
    id_type: ObjectIdType,
    owner: Owner,
) -> CreatedCloudObject {
    let now = ServerTimestamp::new(Utc::now());
    CreatedCloudObject {
        client_id,
        revision_and_editor: RevisionAndLastEditor {
            revision: Revision::from(now),
            last_editor_uid: None,
        },
        metadata_ts: now,
        server_id_and_type: ServerIdAndType {
            id: synthetic_server_id(),
            id_type,
        },
        creator_uid: None,
        permissions: personal_permissions(owner),
    }
}

fn update_success<T>() -> UpdateCloudObjectResult<T> {
    UpdateCloudObjectResult::Success {
        revision_and_editor: RevisionAndLastEditor {
            revision: Revision::from(ServerTimestamp::new(Utc::now())),
            last_editor_uid: None,
        },
    }
}

fn server_metadata(id: ServerId, folder_id: Option<FolderId>) -> ServerMetadata {
    let now = ServerTimestamp::new(Utc::now());
    ServerMetadata {
        uid: id,
        revision: Revision::from(now),
        metadata_last_updated_ts: now,
        trashed_ts: None,
        folder_id,
        is_welcome_object: false,
        creator_uid: None,
        last_editor_uid: None,
        current_editor_uid: None,
    }
}

fn personal_permissions(owner: Owner) -> ServerPermissions {
    ServerPermissions {
        space: owner,
        guests: Vec::new(),
        anyone_link_sharing: None,
        permissions_last_updated_ts: ServerTimestamp::new(Utc::now()),
    }
}

fn local_owner() -> Owner {
    Owner::User {
        user_uid: UserUid::new("warpoca-local"),
    }
}

fn synthetic_server_id() -> ServerId {
    let mut id = Uuid::new_v4().simple().to_string();
    id.truncate(22);
    ServerId::from_string_lossy(id)
}
