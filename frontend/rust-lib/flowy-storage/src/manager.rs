use crate::entities::FileStatePB;
use crate::file_cache::FileTempStorage;
use crate::notification::{make_notification, StorageNotification};
use crate::sqlite_sql::{
  batch_select_upload_file, delete_all_upload_parts, delete_upload_file, insert_upload_file,
  insert_upload_part, is_upload_completed, select_upload_file, select_upload_parts,
  update_upload_file_completed, update_upload_file_upload_id, UploadFilePartTable, UploadFileTable,
};
use crate::uploader::{FileUploader, FileUploaderRunner, Signal, UploadTask, UploadTaskQueue};
use allo_isolate::Isolate;
use async_trait::async_trait;
use collab_importer::util::FileId;
use dashmap::DashMap;
use flowy_error::{ErrorCode, FlowyError, FlowyResult};
use flowy_sqlite::DBConnection;
use flowy_storage_pub::chunked_byte::{calculate_offsets, ChunkedBytes, MIN_CHUNK_SIZE};
use flowy_storage_pub::cloud::StorageCloudService;
use flowy_storage_pub::storage::{
  CompletedPartRequest, CreatedUpload, FileProgress, FileProgressReceiver, FileUploadState,
  ProgressNotifier, StorageService, UploadPartResponse,
};
use lib_infra::box_any::BoxAny;
use lib_infra::isolate_stream::{IsolateSink, SinkExt};
use lib_infra::util::timestamp;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::AsyncWriteExt;
use tokio::sync::{broadcast, watch};
use tracing::{debug, error, info, instrument, trace};

pub trait StorageUserService: Send + Sync + 'static {
  fn user_id(&self) -> Result<i64, FlowyError>;
  fn workspace_id(&self) -> Result<String, FlowyError>;
  fn sqlite_connection(&self, uid: i64) -> Result<DBConnection, FlowyError>;
  fn get_application_root_dir(&self) -> &str;
}

type GlobalNotifier = broadcast::Sender<FileProgress>;
pub struct StorageManager {
  pub storage_service: Arc<dyn StorageService>,
  cloud_service: Arc<dyn StorageCloudService>,
  user_service: Arc<dyn StorageUserService>,
  uploader: Arc<FileUploader>,
  progress_notifiers: Arc<DashMap<String, ProgressNotifier>>,
  global_notifier: GlobalNotifier,
}

impl Drop for StorageManager {
  fn drop(&mut self) {
    info!("[File] StorageManager is dropped");
  }
}

impl StorageManager {
  pub fn new(
    cloud_service: Arc<dyn StorageCloudService>,
    user_service: Arc<dyn StorageUserService>,
  ) -> Self {
    let is_exceed_storage_limit = Arc::new(AtomicBool::new(false));
    let temp_storage_path = PathBuf::from(format!(
      "{}/cache_files",
      user_service.get_application_root_dir()
    ));
    let (global_notifier, _) = broadcast::channel(2000);
    let temp_storage = Arc::new(FileTempStorage::new(temp_storage_path));
    let (notifier, notifier_rx) = watch::channel(Signal::Proceed);
    let task_queue = Arc::new(UploadTaskQueue::new(notifier));
    let progress_notifiers = Arc::new(DashMap::new());
    let storage_service = Arc::new(StorageServiceImpl {
      cloud_service: cloud_service.clone(),
      user_service: user_service.clone(),
      temp_storage,
      task_queue: task_queue.clone(),
      is_exceed_storage_limit: is_exceed_storage_limit.clone(),
      progress_notifiers: progress_notifiers.clone(),
      global_notifier: global_notifier.clone(),
    });

    let uploader = Arc::new(FileUploader::new(
      storage_service.clone(),
      task_queue,
      is_exceed_storage_limit,
    ));
    tokio::spawn(FileUploaderRunner::run(
      Arc::downgrade(&uploader),
      notifier_rx,
    ));

    let weak_uploader = Arc::downgrade(&uploader);
    let cloned_user_service = user_service.clone();
    tokio::spawn(async move {
      // Start uploading after 20 seconds
      tokio::time::sleep(Duration::from_secs(20)).await;
      if let Some(uploader) = weak_uploader.upgrade() {
        if let Err(err) = prepare_upload_task(uploader, cloned_user_service).await {
          error!("prepare upload task failed: {}", err);
        }
      }
    });

    let mut rx = global_notifier.subscribe();
    let weak_notifier = Arc::downgrade(&progress_notifiers);
    tokio::spawn(async move {
      while let Ok(progress) = rx.recv().await {
        if let Some(notifiers) = weak_notifier.upgrade() {
          if let Some(mut notifier) = notifiers.get_mut(&progress.file_id) {
            if progress.progress >= 1.0 {
              let finish = FileUploadState::Finished {
                file_id: progress.file_id,
              };
              notifier.notify(finish).await;
            } else {
              let progress = FileUploadState::Uploading {
                progress: progress.progress,
              };
              notifier.notify(progress).await;
            }
          }
        } else {
          info!("progress notifiers is dropped");
          break;
        }
      }
    });

    Self {
      storage_service,
      cloud_service,
      user_service,
      uploader,
      progress_notifiers,
      global_notifier,
    }
  }

  pub async fn register_file_progress_stream(&self, port: i64) {
    info!("register file progress stream: {}", port);
    let mut sink = IsolateSink::new(Isolate::new(port));
    let mut rx = self.global_notifier.subscribe();
    tokio::spawn(async move {
      while let Ok(progress) = rx.recv().await {
        if let Ok(s) = serde_json::to_string(&progress) {
          if let Err(err) = sink.send(s).await {
            error!("[File]: send file progress failed: {}", err);
          }
        }
      }
    });
  }

  pub async fn query_file_state(&self, url: &str) -> Option<FileStatePB> {
    let (workspace_id, parent_dir, file_id) = self.cloud_service.parse_object_url_v1(url).await?;
    let current_workspace_id = self.user_service.workspace_id().ok()?;
    if workspace_id != current_workspace_id {
      return None;
    }

    let uid = self.user_service.user_id().ok()?;
    let mut conn = self.user_service.sqlite_connection(uid).ok()?;
    let is_finish = is_upload_completed(&mut conn, &workspace_id, &parent_dir, &file_id).ok()?;

    if let Err(err) = self.global_notifier.send(FileProgress::new_progress(
      url.to_string(),
      file_id.clone(),
      if is_finish { 1.0 } else { 0.0 },
    )) {
      error!("[File] send global notifier failed: {}", err);
    }

    Some(FileStatePB { file_id, is_finish })
  }

  pub async fn initialize(&self, _workspace_id: &str) {
    self.enable_storage_write_access();
  }

  pub fn update_network_reachable(&self, reachable: bool) {
    if reachable {
      self.uploader.resume();
    } else {
      self.uploader.pause();
    }
  }

  pub fn disable_storage_write_access(&self) {
    // when storage is purchased, resume the uploader
    self.uploader.disable_storage_write();
  }

  pub fn enable_storage_write_access(&self) {
    // when storage is purchased, resume the uploader
    self.uploader.enable_storage_write();
  }

  pub async fn subscribe_file_state(
    &self,
    parent_dir: &str,
    file_id: &str,
  ) -> Result<Option<FileProgressReceiver>, FlowyError> {
    self
      .storage_service
      .subscribe_file_progress(parent_dir, file_id)
      .await
  }

  pub async fn get_file_state(&self, file_id: &str) -> Option<FileUploadState> {
    self
      .progress_notifiers
      .get(file_id)
      .and_then(|notifier| notifier.value().current_value.clone())
  }
}

async fn prepare_upload_task(
  uploader: Arc<FileUploader>,
  user_service: Arc<dyn StorageUserService>,
) -> FlowyResult<()> {
  let uid = user_service.user_id()?;
  let conn = user_service.sqlite_connection(uid)?;
  let upload_files = batch_select_upload_file(conn, 100, false)?;
  let tasks = upload_files
    .into_iter()
    .map(|upload_file| UploadTask::BackgroundTask {
      workspace_id: upload_file.workspace_id,
      file_id: upload_file.file_id,
      parent_dir: upload_file.parent_dir,
      created_at: upload_file.created_at,
      retry_count: 0,
    })
    .collect::<Vec<_>>();
  info!("[File] prepare upload task: {}", tasks.len());
  uploader.queue_tasks(tasks).await;
  Ok(())
}

pub struct StorageServiceImpl {
  cloud_service: Arc<dyn StorageCloudService>,
  user_service: Arc<dyn StorageUserService>,
  temp_storage: Arc<FileTempStorage>,
  task_queue: Arc<UploadTaskQueue>,
  is_exceed_storage_limit: Arc<AtomicBool>,
  progress_notifiers: Arc<DashMap<String, ProgressNotifier>>,
  global_notifier: GlobalNotifier,
}

#[async_trait]
impl StorageService for StorageServiceImpl {
  fn delete_object(&self, url: String, local_file_path: String) -> FlowyResult<()> {
    let cloud_service = self.cloud_service.clone();
    tokio::spawn(async move {
      match tokio::fs::remove_file(&local_file_path).await {
        Ok(_) => {
          debug!("[File] deleted file from local disk: {}", local_file_path)
        },
        Err(err) => {
          error!("[File] delete file at {} failed: {}", local_file_path, err);
        },
      }
      if let Err(e) = cloud_service.delete_object(&url).await {
        // TODO: add WAL to log the delete operation.
        // keep a list of files to be deleted, and retry later
        error!("[File] delete file failed: {}", e);
      }
      debug!("[File] deleted file from cloud: {}", url);
    });
    Ok(())
  }

  fn download_object(&self, url: String, local_file_path: String) -> FlowyResult<()> {
    let cloud_service = self.cloud_service.clone();
    tokio::spawn(async move {
      if tokio::fs::metadata(&local_file_path).await.is_ok() {
        tracing::warn!("file already exist in user local disk: {}", local_file_path);
        return Ok(());
      }
      let object_value = cloud_service.get_object(url).await?;
      let mut file = tokio::fs::OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .open(&local_file_path)
        .await?;

      match file.write(&object_value.raw).await {
        Ok(n) => {
          info!("downloaded {} bytes to file: {}", n, local_file_path);
        },
        Err(err) => {
          error!("write file failed: {}", err);
        },
      }
      Ok::<_, FlowyError>(())
    });
    Ok(())
  }

  async fn create_upload(
    &self,
    workspace_id: &str,
    parent_dir: &str,
    file_path: &str,
    upload_immediately: bool,
  ) -> Result<(CreatedUpload, Option<FileProgressReceiver>), FlowyError> {
    if workspace_id.is_empty() {
      return Err(FlowyError::internal().with_context("workspace id is empty"));
    }

    if parent_dir.is_empty() {
      return Err(FlowyError::internal().with_context("parent dir is empty"));
    }

    if file_path.is_empty() {
      return Err(FlowyError::internal().with_context("local file path is empty"));
    }

    let workspace_id = workspace_id.to_string();
    let parent_dir = parent_dir.to_string();
    let file_path = file_path.to_string();

    let is_exceed_limit = self
      .is_exceed_storage_limit
      .load(std::sync::atomic::Ordering::Relaxed);
    if is_exceed_limit {
      make_notification(StorageNotification::FileStorageLimitExceeded)
        .payload(FlowyError::file_storage_limit())
        .send();

      return Err(FlowyError::file_storage_limit());
    }

    let local_file_path = self
      .temp_storage
      .create_temp_file_from_existing(Path::new(&file_path))
      .await
      .map_err(|err| {
        error!("[File] create temp file failed: {}", err);
        FlowyError::internal()
          .with_context(format!("create temp file for upload file failed: {}", err))
      })?;

    // 1. create a file record and chunk the file
    let record = create_upload_record(workspace_id, parent_dir, local_file_path.clone()).await?;
    // 2. save the record to sqlite
    let conn = self
      .user_service
      .sqlite_connection(self.user_service.user_id()?)?;
    let url = self
      .cloud_service
      .get_object_url_v1(&record.workspace_id, &record.parent_dir, &record.file_id)
      .await?;
    let file_id = record.file_id.clone();
    match insert_upload_file(conn, &record) {
      Ok(_) => {
        // 3. generate url for given file
        if upload_immediately {
          self
            .task_queue
            .queue_task(UploadTask::ImmediateTask {
              local_file_path,
              record,
              retry_count: 3,
            })
            .await;
        } else {
          self
            .task_queue
            .queue_task(UploadTask::Task {
              local_file_path,
              record,
              retry_count: 0,
            })
            .await;
        }

        let notifier = ProgressNotifier::new(file_id.to_string());
        let receiver = notifier.subscribe();
        self
          .progress_notifiers
          .insert(file_id.to_string(), notifier);
        Ok::<_, FlowyError>((CreatedUpload { url, file_id }, Some(receiver)))
      },
      Err(err) => {
        if matches!(err.code, ErrorCode::DuplicateSqliteRecord) {
          info!("[File] upload record already exists, skip creating new upload task");
          Ok::<_, FlowyError>((CreatedUpload { url, file_id }, None))
        } else {
          Err(err)
        }
      },
    }
  }

  async fn start_upload(&self, record: &BoxAny) -> Result<(), FlowyError> {
    let file_record = record.downcast_ref::<UploadFileTable>().ok_or_else(|| {
      FlowyError::internal().with_context("failed to downcast record to UploadFileTable")
    })?;

    start_upload(
      &self.cloud_service,
      &self.user_service,
      &self.temp_storage,
      file_record,
      self.global_notifier.clone(),
    )
    .await?;

    Ok(())
  }

  async fn resume_upload(
    &self,
    workspace_id: &str,
    parent_dir: &str,
    file_id: &str,
  ) -> Result<(), FlowyError> {
    // Gathering the upload record and parts from the sqlite database.
    let mut conn = self
      .user_service
      .sqlite_connection(self.user_service.user_id()?)?;

    if let Some(upload_file) = select_upload_file(&mut conn, workspace_id, parent_dir, file_id)? {
      resume_upload(
        &self.cloud_service,
        &self.user_service,
        &self.temp_storage,
        upload_file,
        self.global_notifier.clone(),
      )
      .await?;
    } else {
      error!("[File] resume upload failed: record not found");
    }
    Ok(())
  }

  async fn subscribe_file_progress(
    &self,
    parent_idr: &str,
    file_id: &str,
  ) -> Result<Option<FileProgressReceiver>, FlowyError> {
    trace!("[File]: subscribe file progress: {}", file_id);

    let is_completed = {
      let mut conn = self
        .user_service
        .sqlite_connection(self.user_service.user_id()?)?;
      let workspace_id = self.user_service.workspace_id()?;
      is_upload_completed(&mut conn, &workspace_id, parent_idr, file_id).unwrap_or(false)
    };

    if is_completed {
      return Ok(None);
    }

    let notifier = self
      .progress_notifiers
      .entry(file_id.to_string())
      .or_insert_with(|| ProgressNotifier::new(file_id.to_string()));
    Ok(Some(notifier.subscribe()))
  }
}

async fn create_upload_record(
  workspace_id: String,
  parent_dir: String,
  local_file_path: String,
) -> FlowyResult<UploadFileTable> {
  let file_path = Path::new(&local_file_path);
  let file = tokio::fs::File::open(&file_path).await?;
  let metadata = file.metadata().await?;
  let file_size = metadata.len() as usize;

  // Calculate the total number of chunks
  let num_chunk = calculate_offsets(file_size, MIN_CHUNK_SIZE).len();
  let content_type = mime_guess::from_path(&file_path)
    .first_or_octet_stream()
    .to_string();
  let file_id = FileId::from_path(&file_path.to_path_buf()).await?;
  let record = UploadFileTable {
    workspace_id,
    file_id,
    // When the upload_id is empty string, we will create a new upload using [Self::start_upload] method
    upload_id: "".to_string(),
    parent_dir,
    local_file_path,
    content_type,
    chunk_size: MIN_CHUNK_SIZE as i32,
    num_chunk: num_chunk as i32,
    created_at: timestamp(),
    is_finish: false,
  };
  Ok(record)
}

#[instrument(level = "debug", skip_all, err)]
async fn start_upload(
  cloud_service: &Arc<dyn StorageCloudService>,
  user_service: &Arc<dyn StorageUserService>,
  temp_storage: &Arc<FileTempStorage>,
  upload_file: &UploadFileTable,
  global_notifier: GlobalNotifier,
) -> FlowyResult<()> {
  // 4. gather existing completed parts
  let mut conn = user_service.sqlite_connection(user_service.user_id()?)?;
  let mut completed_parts = select_upload_parts(&mut conn, &upload_file.upload_id)
    .unwrap_or_default()
    .into_iter()
    .map(|part| CompletedPartRequest {
      e_tag: part.e_tag,
      part_number: part.part_num,
    })
    .collect::<Vec<_>>();
  let upload_offset = completed_parts.len() as u64;

  let file_path = Path::new(&upload_file.local_file_path);
  if !file_path.exists() {
    error!("[File] file not found: {}", upload_file.local_file_path);
    if let Ok(uid) = user_service.user_id() {
      if let Ok(conn) = user_service.sqlite_connection(uid) {
        delete_upload_file(conn, &upload_file.upload_id)?;
      }
    }
  }

  let mut chunked_bytes =
    ChunkedBytes::from_file(&upload_file.local_file_path, MIN_CHUNK_SIZE).await?;
  let total_parts = chunked_bytes.total_chunks();
  if let Err(err) = chunked_bytes.set_offset(upload_offset).await {
    error!(
      "[File] set offset failed: {} for file: {}",
      err, upload_file.local_file_path
    );
    if let Ok(uid) = user_service.user_id() {
      if let Ok(conn) = user_service.sqlite_connection(uid) {
        delete_upload_file(conn, &upload_file.upload_id)?;
      }
    }
  }

  info!(
    "[File] start upload: workspace: {}, parent_dir: {}, file_id: {}, chunk: {}",
    upload_file.workspace_id, upload_file.parent_dir, upload_file.file_id, chunked_bytes,
  );

  let mut upload_file = upload_file.clone();
  // 1. create upload
  trace!(
    "[File] create upload for workspace: {}, parent_dir: {}, file_id: {}",
    upload_file.workspace_id,
    upload_file.parent_dir,
    upload_file.file_id
  );

  let create_upload_resp_result = cloud_service
    .create_upload(
      &upload_file.workspace_id,
      &upload_file.parent_dir,
      &upload_file.file_id,
      &upload_file.content_type,
    )
    .await;
  if let Err(err) = create_upload_resp_result.as_ref() {
    handle_upload_error(user_service, &err, &upload_file.upload_id);
  }
  let create_upload_resp = create_upload_resp_result?;

  // 2. update upload_id
  let conn = user_service.sqlite_connection(user_service.user_id()?)?;
  update_upload_file_upload_id(
    conn,
    &upload_file.workspace_id,
    &upload_file.parent_dir,
    &upload_file.file_id,
    &create_upload_resp.upload_id,
  )?;

  trace!(
    "[File] {} update upload_id: {}",
    upload_file.file_id,
    create_upload_resp.upload_id
  );
  upload_file.upload_id = create_upload_resp.upload_id;

  // 3. start uploading parts
  info!(
    "[File] {} start uploading parts:{}, offset:{}",
    upload_file.file_id,
    chunked_bytes.total_chunks(),
    upload_offset,
  );

  let mut part_number = upload_offset + 1;
  while let Some(chunk_result) = chunked_bytes.next_chunk().await {
    match chunk_result {
      Ok(chunk_bytes) => {
        info!(
          "[File] {} uploading {}th part, size:{}KB",
          upload_file.file_id,
          part_number,
          chunk_bytes.len() / 1000,
        );

        let file_url = cloud_service
          .get_object_url_v1(
            &upload_file.workspace_id,
            &upload_file.parent_dir,
            &upload_file.file_id,
          )
          .await?;
        // start uploading parts
        match upload_part(
          cloud_service,
          user_service,
          &upload_file.workspace_id,
          &upload_file.parent_dir,
          &upload_file.upload_id,
          &upload_file.file_id,
          part_number as i32,
          chunk_bytes.to_vec(),
        )
        .await
        {
          Ok(resp) => {
            trace!(
              "[File] {} part {} uploaded",
              upload_file.file_id,
              part_number
            );
            let mut progress_value = (part_number as f64 / total_parts as f64).clamp(0.0, 1.0);
            // The 0.1 is reserved for the complete_upload progress
            if progress_value >= 0.9 {
              progress_value = 0.9;
            }
            let progress =
              FileProgress::new_progress(file_url, upload_file.file_id.clone(), progress_value);
            trace!("[File] upload progress: {}", progress);

            if let Err(err) = global_notifier.send(progress) {
              error!("[File] send global notifier failed: {}", err);
            }

            // gather completed part
            completed_parts.push(CompletedPartRequest {
              e_tag: resp.e_tag,
              part_number: resp.part_num,
            });
          },
          Err(err) => {
            error!(
              "[File] {} failed to upload part: {}",
              upload_file.file_id, err
            );
            handle_upload_error(user_service, &err, &upload_file.upload_id);
            if let Err(err) = global_notifier.send(FileProgress::new_error(
              file_url,
              upload_file.file_id.clone(),
              err.msg.clone(),
            )) {
              error!("[File] send global notifier failed: {}", err);
            }
            return Err(err);
          },
        }
        part_number += 1; // Increment part number
      },
      Err(e) => {
        error!(
          "[File] {} failed to read chunk: {:?}",
          upload_file.file_id, e
        );
        break;
      },
    }
  }

  // mark it as completed
  let complete_upload_result = complete_upload(
    cloud_service,
    user_service,
    temp_storage,
    &upload_file,
    completed_parts,
    &global_notifier,
  )
  .await;
  if let Err(err) = complete_upload_result {
    handle_upload_error(user_service, &err, &upload_file.upload_id);
    return Err(err);
  }

  Ok(())
}

fn handle_upload_error(
  user_service: &Arc<dyn StorageUserService>,
  err: &FlowyError,
  upload_id: &str,
) {
  if err.is_file_limit_exceeded() {
    make_notification(StorageNotification::FileStorageLimitExceeded)
      .payload(err.clone())
      .send();
  }

  if err.is_single_file_limit_exceeded() {
    info!("[File] file exceed limit:{}", upload_id);
    if let Ok(user_id) = user_service.user_id() {
      if let Ok(db_conn) = user_service.sqlite_connection(user_id) {
        if let Err(err) = delete_upload_file(db_conn, upload_id) {
          error!("[File] delete upload file:{} error:{}", upload_id, err);
        }
      }
    }

    make_notification(StorageNotification::SingleFileLimitExceeded)
      .payload(err.clone())
      .send();
  }
}

#[instrument(level = "debug", skip_all, err)]
async fn resume_upload(
  cloud_service: &Arc<dyn StorageCloudService>,
  user_service: &Arc<dyn StorageUserService>,
  temp_storage: &Arc<FileTempStorage>,
  upload_file: UploadFileTable,
  global_notifier: GlobalNotifier,
) -> FlowyResult<()> {
  trace!(
    "[File] resume upload for workspace: {}, parent_dir: {}, file_id: {}, local_file_path:{}",
    upload_file.workspace_id,
    upload_file.parent_dir,
    upload_file.file_id,
    upload_file.local_file_path
  );

  start_upload(
    cloud_service,
    user_service,
    temp_storage,
    &upload_file,
    global_notifier,
  )
  .await?;

  Ok(())
}

#[allow(clippy::too_many_arguments)]
#[instrument(level = "debug", skip_all)]
async fn upload_part(
  cloud_service: &Arc<dyn StorageCloudService>,
  user_service: &Arc<dyn StorageUserService>,
  workspace_id: &str,
  parent_dir: &str,
  upload_id: &str,
  file_id: &str,
  part_number: i32,
  body: Vec<u8>,
) -> Result<UploadPartResponse, FlowyError> {
  let resp = cloud_service
    .upload_part(
      workspace_id,
      parent_dir,
      upload_id,
      file_id,
      part_number,
      body,
    )
    .await?;

  // save uploaded part to sqlite
  let conn = user_service.sqlite_connection(user_service.user_id()?)?;
  insert_upload_part(
    conn,
    &UploadFilePartTable {
      upload_id: upload_id.to_string(),
      e_tag: resp.e_tag.clone(),
      part_num: resp.part_num,
    },
  )?;

  Ok(resp)
}

async fn complete_upload(
  cloud_service: &Arc<dyn StorageCloudService>,
  user_service: &Arc<dyn StorageUserService>,
  temp_storage: &Arc<FileTempStorage>,
  upload_file: &UploadFileTable,
  parts: Vec<CompletedPartRequest>,
  global_notifier: &GlobalNotifier,
) -> Result<(), FlowyError> {
  let file_url = cloud_service
    .get_object_url_v1(
      &upload_file.workspace_id,
      &upload_file.parent_dir,
      &upload_file.file_id,
    )
    .await?;

  info!(
    "[File]: completing file upload: {}, num parts: {}, url:{}",
    upload_file.file_id,
    parts.len(),
    file_url
  );
  match cloud_service
    .complete_upload(
      &upload_file.workspace_id,
      &upload_file.parent_dir,
      &upload_file.upload_id,
      &upload_file.file_id,
      parts,
    )
    .await
  {
    Ok(_) => {
      info!("[File] completed upload file: {}", upload_file.file_id);
      let progress = FileProgress::new_progress(file_url, upload_file.file_id.clone(), 1.0);
      info!(
        "[File]: notify upload progress:{}, {}",
        upload_file.file_id, progress
      );

      if let Err(err) = global_notifier.send(progress) {
        error!("[File] send global notifier failed: {}", err);
      }

      let conn = user_service.sqlite_connection(user_service.user_id()?)?;
      update_upload_file_completed(conn, &upload_file.upload_id)?;

      if let Err(err) = temp_storage
        .delete_temp_file(&upload_file.local_file_path)
        .await
      {
        trace!("[File] delete temp file failed: {}", err);
      }
    },
    Err(err) => {
      error!("[File] complete upload failed: {}", err);

      let progress =
        FileProgress::new_error(file_url, upload_file.file_id.clone(), err.msg.clone());
      if let Err(send_err) = global_notifier.send(progress) {
        error!("[File] send global notifier failed: {}", send_err);
      }

      let conn = user_service.sqlite_connection(user_service.user_id()?)?;
      if let Err(err) = delete_all_upload_parts(conn, &upload_file.upload_id) {
        error!("[File] delete all upload parts failed: {}", err);
      }
      return Err(err);
    },
  }
  Ok(())
}
