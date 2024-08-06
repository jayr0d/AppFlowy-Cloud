use actix_web::web::Bytes;
use anyhow::anyhow;
use std::collections::HashMap;

use app_error::AppError;
use appflowy_ai_client::client::AppFlowyAIClient;
use async_stream::stream;
use database::chat;
use database::chat::chat_ops::{
  delete_answer_message_by_question_message_id, insert_answer_message,
  insert_answer_message_with_transaction, insert_chat, insert_question_message,
  select_chat_messages,
};
use database_entity::dto::{
  ChatAuthor, ChatAuthorType, ChatMessage, ChatMessageType, CreateChatMessageParams,
  CreateChatParams, GetChatMessageParams, RepeatedChatMessage, UpdateChatMessageContentParams,
};
use futures::stream::Stream;
use serde_json::Value;
use sqlx::PgPool;
use tracing::{error, info, trace};

use appflowy_ai_client::dto::AIModel;
use validator::Validate;

pub(crate) async fn create_chat(
  pg_pool: &PgPool,
  params: CreateChatParams,
  workspace_id: &str,
) -> Result<(), AppError> {
  params.validate()?;

  let mut txn = pg_pool.begin().await?;
  insert_chat(&mut txn, workspace_id, params).await?;
  txn.commit().await?;
  Ok(())
}

pub(crate) async fn delete_chat(pg_pool: &PgPool, chat_id: &str) -> Result<(), AppError> {
  let mut txn = pg_pool.begin().await?;
  chat::chat_ops::delete_chat(&mut txn, chat_id).await?;
  txn.commit().await?;
  Ok(())
}

pub async fn update_chat_message(
  pg_pool: &PgPool,
  params: UpdateChatMessageContentParams,
  ai_client: AppFlowyAIClient,
  ai_model: AIModel,
) -> Result<(), AppError> {
  let mut txn = pg_pool.begin().await?;
  delete_answer_message_by_question_message_id(&mut txn, params.message_id).await?;
  chat::chat_ops::update_chat_message_content(&mut txn, &params).await?;
  txn.commit().await.map_err(|err| {
    AppError::Internal(anyhow!(
      "Failed to commit transaction to update chat message: {}",
      err
    ))
  })?;

  let new_answer = ai_client
    .send_question(&params.chat_id, &params.content, &ai_model)
    .await?;
  let _answer = insert_answer_message(
    pg_pool,
    ChatAuthor::ai(),
    &params.chat_id,
    new_answer.content,
    new_answer.metadata,
    params.message_id,
  )
  .await?;

  Ok(())
}

pub async fn generate_chat_message_answer(
  pg_pool: &PgPool,
  ai_client: AppFlowyAIClient,
  question_message_id: i64,
  chat_id: &str,
  ai_model: AIModel,
) -> Result<ChatMessage, AppError> {
  let (content, metadata) =
    chat::chat_ops::select_chat_message_content(pg_pool, question_message_id).await?;
  let new_answer = ai_client
    .send_question(chat_id, &content, Some(metadata), &ai_model)
    .await?;

  info!("new_answer: {:?}", new_answer);
  // Save the answer to the database
  let mut txn = pg_pool.begin().await?;
  let message = insert_answer_message_with_transaction(
    &mut txn,
    ChatAuthor::ai(),
    chat_id,
    new_answer.content,
    new_answer.metadata.unwrap_or_default(),
    question_message_id,
  )
  .await?;
  txn.commit().await.map_err(|err| {
    AppError::Internal(anyhow!(
      "Failed to commit transaction to update chat message: {}",
      err
    ))
  })?;

  Ok(message)
}

pub async fn create_chat_message(
  pg_pool: &PgPool,
  uid: i64,
  chat_id: String,
  params: CreateChatMessageParams,
) -> Result<ChatMessage, AppError> {
  let params = params.clone();
  let chat_id = chat_id.clone();
  let pg_pool = pg_pool.clone();

  let question = insert_question_message(
    &pg_pool,
    ChatAuthor::new(uid, ChatAuthorType::Human),
    &chat_id,
    params.content.clone(),
    params.metadata,
  )
  .await?;
  Ok(question)
}

enum ContextType {
  Unknown,
  Text,
}

/// Extracts the chat context from the metadata. Currently, we only support text as a context. In
/// the future, we will support other types of context.
pub(crate) enum ExtractChatMetadata {
  Text {
    text: String,
    metadata: HashMap<String, Value>,
  },
}
/// Removes the "content" field from the metadata if the "ty" field is equal to "text".
/// The metadata struct is shown below:
/// {
///   "data": {
///       "content": "hello world"
///       "size": 122,
///       "content_type": "text",
///   },
///   "id": "id",
///   "name": "name"
/// }
///
/// # Parameters
/// - `params`: A mutable reference to `CreateChatMessageParams` which contains metadata.
///
/// # Returns
/// - `Option<(String, HashMap<String, serde_json::Value>)>`: A tuple containing the removed content and the updated metadata, otherwise `None`.
fn extract_message_metadata(
  message_metadata: &mut serde_json::Value,
) -> Option<ExtractChatMetadata> {
  trace!("Extracting metadata: {:?}", message_metadata);

  if let Value::Object(message_metadata) = message_metadata {
    let mut context_type = ContextType::Unknown;
    if let Some(Value::Object(data)) = message_metadata.get("data") {
      if let Some(ty) = data.get("content_type").and_then(|v| v.as_str()) {
        match ty {
          "text" => context_type = ContextType::Text,
          _ => context_type = ContextType::Unknown,
        }
      }
    }

    match context_type {
      ContextType::Unknown => {
        // do nothing
      },
      ContextType::Text => {
        // remove the "data" field from the context if the "ty" field is equal to "text"
        let mut text = None;
        if let Some(Value::Object(ref mut data)) = message_metadata.remove("data") {
          let content = data
            .remove("content")
            .and_then(|value| {
              if let Value::String(s) = value {
                Some(s)
              } else {
                None
              }
            })
            .unwrap_or_default();

          let content_size = data
            .remove("size")
            .and_then(|value| {
              if let Value::Number(n) = value {
                n.as_i64()
              } else {
                None
              }
            })
            .unwrap_or(0);

          // If the content is not empty and the content size is equal to the length of the content
          if !content.is_empty() && content.len() == content_size as usize {
            text = Some(content);
          }
        }

        return text.map(|text| ExtractChatMetadata::Text {
          text,
          metadata: message_metadata.clone().into_iter().collect(),
        });
      },
    }
  }

  None
}

pub(crate) fn extract_chat_message_metadata(
  params: &mut CreateChatMessageParams,
) -> Vec<ExtractChatMetadata> {
  let mut extract_metadatas = vec![];
  if let Some(Value::Array(ref mut list)) = params.metadata {
    trace!("Extracting chat metadata: {:?}", list);
    for metadata in list {
      if let Some(extract_context) = extract_message_metadata(metadata) {
        extract_metadatas.push(extract_context);
      }
    }
  }

  extract_metadatas
}

pub async fn create_chat_message_stream(
  pg_pool: &PgPool,
  uid: i64,
  chat_id: String,
  params: CreateChatMessageParams,
  ai_client: AppFlowyAIClient,
  ai_model: AIModel,
) -> impl Stream<Item = Result<Bytes, AppError>> {
  let params = params.clone();
  let chat_id = chat_id.clone();
  let pg_pool = pg_pool.clone();
  let stream = stream! {
      // Insert question message
      let question = match insert_question_message(
          &pg_pool,
          ChatAuthor::new(uid, ChatAuthorType::Human),
          &chat_id,
          params.content.clone(),
          params.metadata,
      ).await {
          Ok(question) => question,
          Err(err) => {
              error!("Failed to insert question message: {}", err);
              yield Err(err);
              return;
          }
      };

      let question_id = question.message_id;
      let question_bytes = match serde_json::to_vec(&question) {
          Ok(s) => Bytes::from(s),
          Err(err) => {
              error!("Failed to serialize question message: {}", err);
              yield Err(AppError::from(err));
              return;
          }
      };

      yield Ok::<Bytes, AppError>(question_bytes);

      // Insert answer message
      match params.message_type {
          ChatMessageType::System => {}
          ChatMessageType::User => {
              let answer = match ai_client.send_question(&chat_id, &params.content, &ai_model).await {
                  Ok(response) => response,
                  Err(err) => {
                      error!("Failed to send question to AI: {}", err);
                      yield Err(AppError::from(err));
                      return;
                  }
              };

              let answer = match insert_answer_message(&pg_pool, ChatAuthor::ai(), &chat_id, answer.content, answer.metadata,question_id).await {
                  Ok(answer) => answer,
                  Err(err) => {
                      error!("Failed to insert answer message: {}", err);
                      yield Err(err);
                      return;
                  }
              };

              let answer_bytes = match serde_json::to_vec(&answer) {
                  Ok(s) => Bytes::from(s),
                  Err(err) => {
                      error!("Failed to serialize answer message: {}", err);
                      yield Err(AppError::from(err));
                      return;
                  }
              };

              yield Ok::<Bytes, AppError>(answer_bytes);
          }
      }
  };

  stream
}

pub async fn get_chat_messages(
  pg_pool: &PgPool,
  params: GetChatMessageParams,
  chat_id: &str,
) -> Result<RepeatedChatMessage, AppError> {
  params.validate()?;

  let mut txn = pg_pool.begin().await?;
  let messages = select_chat_messages(&mut txn, chat_id, params).await?;
  txn.commit().await?;
  Ok(messages)
}
