use async_recursion::async_recursion;
use futures::StreamExt;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::ops::Add;
use std::sync::{Arc, RwLock};
use std::time::Duration;
use telegram_bot::JsonIdResponse;
use telegram_bot::JsonRequestType;
use telegram_bot::{
    Api, ChatId, ChatMemberStatus, ChatRef, Document, EditMessageText, GetChatMember, GetFile,
    HttpRequest, Integer, KeyboardButton, KickChatMember, Message, MessageId, MessageOrChannelPost,
    ParseMode, ReplyKeyboardMarkup, ReplyKeyboardRemove, ReplyMarkup, Request, RequestType,
    RequestUrl, ResponseType, SendMessage, ToChatRef, UpdateKind, User, UserId,
};
use tokio::sync::mpsc::unbounded_channel;
use tokio::time::Instant;
use tokio_stream::wrappers::UnboundedReceiverStream;

#[derive(Deserialize)]
struct ChatInviteLink {
    invite_link: String,
    #[allow(dead_code)]
    creator: User,
    #[allow(dead_code)]
    creates_join_request: bool,
    #[allow(dead_code)]
    is_primary: bool,
    #[allow(dead_code)]
    is_revoked: bool,
    #[allow(dead_code)]
    name: Option<String>,
    #[allow(dead_code)]
    expire_date: Option<Integer>,
    #[allow(dead_code)]
    member_limit: Option<Integer>,
    #[allow(dead_code)]
    pending_join_request_count: Option<Integer>,
}

#[derive(Debug, Clone, PartialEq, PartialOrd, Serialize)]
struct RevokeChatInviteLink {
    chat_id: ChatRef,
    invite_link: String,
}

impl RevokeChatInviteLink {
    pub fn new(chat_id: ChatId, invite_link: String) -> Self {
        RevokeChatInviteLink {
            chat_id: chat_id.to_chat_ref(),
            invite_link,
        }
    }
}

impl Request for RevokeChatInviteLink {
    type Type = JsonRequestType<Self>;
    type Response = JsonIdResponse<ChatInviteLink>;

    fn serialize(&self) -> Result<HttpRequest, telegram_bot::types::Error> {
        <Self::Type as RequestType>::serialize(RequestUrl::method("revokeChatInviteLink"), self)
    }
}

#[derive(Debug, Clone, PartialEq, PartialOrd, Serialize)]
struct CreateChatInviteLink {
    chat_id: ChatRef,
    name: Option<String>,
    expire_date: Option<Integer>,
    member_limit: Option<Integer>,
    creates_join_request: Option<bool>,
}

impl CreateChatInviteLink {
    pub fn new(chat_id: ChatId) -> Self {
        CreateChatInviteLink {
            chat_id: chat_id.to_chat_ref(),
            name: None,
            expire_date: None,
            member_limit: None,
            creates_join_request: None,
        }
    }
}

impl Request for CreateChatInviteLink {
    type Type = JsonRequestType<Self>;
    type Response = JsonIdResponse<ChatInviteLink>;

    fn serialize(&self) -> Result<HttpRequest, telegram_bot::types::Error> {
        <Self::Type as RequestType>::serialize(RequestUrl::method("createChatInviteLink"), self)
    }
}

#[derive(Clone)]
pub struct TelegramBot {
    token: String,
    api: Api,
    next_time_slot: Arc<RwLock<HashMap<ChatId, Instant>>>,
}

impl TelegramBot {
    const MAX_LEN: usize = 4096;
    const TRIES: u8 = 20;

    pub fn new(token: String) -> (TelegramBot, UnboundedReceiverStream<Message>) {
        let api = Api::new(token.clone());
        let mut stream = api.stream();
        let (sender, receiver) = unbounded_channel();

        tokio::spawn(async move {
            while let Some(update) = stream.next().await {
                match update {
                    Ok(update) => match update.kind {
                        UpdateKind::Message(message) => match sender.send(message) {
                            Ok(_) => {}
                            Err(err) => {
                                panic!("Error with sending update: {}", err);
                            }
                        },
                        _ => {}
                    },
                    Err(err) => {
                        log::error!("Error with update: {}", err);
                    }
                };
            }
        });

        (
            TelegramBot {
                token,
                api,
                next_time_slot: Arc::new(RwLock::new(HashMap::new())),
            },
            UnboundedReceiverStream::new(receiver),
        )
    }

    #[async_recursion]
    pub async fn send_message(
        &self,
        chat_id: ChatId,
        text: String,
        keyboard_options: KeyboardOptions,
    ) -> Option<MessageId> {
        let text = text.as_str().chars().collect::<Vec<_>>();
        if text.len() > Self::MAX_LEN {
            self.send_message(
                chat_id,
                text[..Self::MAX_LEN].iter().cloned().collect::<String>(),
                keyboard_options,
            )
            .await;
            self.send_message(
                chat_id,
                text[Self::MAX_LEN..].iter().cloned().collect::<String>(),
                keyboard_options,
            )
            .await;
            return None;
        }
        let text = text.iter().cloned().collect::<String>();
        self.wait_for_slot(chat_id).await;
        self.block_slot(chat_id);
        let mut request = SendMessage::new(chat_id, text.clone());
        request.parse_mode(ParseMode::Html);
        let reply_markup = keyboard_options.reply_markup();
        if let Some(reply_markup) = reply_markup {
            request.reply_markup(reply_markup);
        }
        let message = self.send_request(request).await;
        self.save_slot(chat_id);
        match message {
            MessageOrChannelPost::Message(message) => Some(message.id),
            MessageOrChannelPost::ChannelPost(_) => unreachable!(),
        }
    }

    pub fn try_send_message(&self, chat_id: ChatId, message: String) {
        let message = message.as_str().chars().collect::<Vec<_>>();
        let bot = self.clone();
        tokio::spawn(async move {
            for i in (0..message.len()).step_by(Self::MAX_LEN) {
                for _ in 0..Self::TRIES {
                    let mut msg = SendMessage::new(
                        chat_id,
                        message[i..(i + Self::MAX_LEN).min(message.len())]
                            .iter()
                            .cloned()
                            .collect::<String>(),
                    );
                    msg.parse_mode(ParseMode::Html);
                    match bot.api.send(msg).await {
                        Ok(_) => {
                            break;
                        }
                        Err(err) => {
                            log::error!("Error sending optional message: {}", err);
                            tokio::time::sleep(Duration::from_secs(1)).await;
                        }
                    }
                }
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
        });
    }

    pub async fn edit_message(&self, chat_id: ChatId, message_id: MessageId, text: String) {
        let mut edit_message_text = EditMessageText::new(chat_id, message_id, text.clone());
        edit_message_text.parse_mode(ParseMode::Html);
        self.send_request(edit_message_text).await;
    }

    pub async fn get_file(&self, document: Document) -> Option<(String, String)> {
        let doc = document.clone();
        if document.file_name.is_none()
            || document
                .mime_type
                .unwrap_or_else(|| "".to_string())
                .starts_with("text")
        {
            return None;
        }
        let file = self.send_request(GetFile::new(doc.clone())).await;
        let url = file.get_url(self.token.as_str());
        if url.is_none() {
            return None;
        }
        let url = url.unwrap();
        download_content(url)
            .await
            .map(|content| (document.file_name.unwrap(), content))
    }

    pub async fn kick(&self, chat_id: ChatId, user_id: UserId) {
        self.wait_for_slot(chat_id).await;
        self.block_slot(chat_id);
        let status = self
            .send_request(GetChatMember::new(chat_id, user_id))
            .await;
        if status.status == ChatMemberStatus::Member {
            self.send_request(KickChatMember::new(chat_id, user_id))
                .await;
        }
        self.save_slot(chat_id);
    }

    pub async fn invalidate_invite_link(&self, chat_id: ChatId, invite_link: String) {
        self.wait_for_slot(chat_id).await;
        self.block_slot(chat_id);
        self.send_request(RevokeChatInviteLink::new(chat_id, invite_link))
            .await;
        self.save_slot(chat_id);
    }

    pub async fn create_invite_link(&self, chat_id: ChatId) -> String {
        self.wait_for_slot(chat_id).await;
        self.block_slot(chat_id);
        let res = self
            .send_request(CreateChatInviteLink::new(chat_id))
            .await
            .invite_link;
        self.save_slot(chat_id);
        res
    }

    async fn wait_for_slot(&self, chat_id: ChatId) {
        let now = Instant::now();
        // let guard = ;
        let next_time_slot = self
            .next_time_slot
            .read()
            .unwrap()
            .get(&chat_id)
            .unwrap_or(&now)
            .clone();
        if next_time_slot > now {
            tokio::time::sleep_until(next_time_slot).await;
        }
    }

    fn save_slot(&self, chat_id: ChatId) {
        let mut guard = self.next_time_slot.write().unwrap();
        guard.insert(chat_id, Instant::now().add(Duration::from_secs(1)));
    }

    fn block_slot(&self, chat_id: ChatId) {
        let mut guard = self.next_time_slot.write().unwrap();
        guard.insert(chat_id, Instant::now().add(Duration::from_secs(100)));
    }

    async fn send_request<Req: Request + Clone>(
        &self,
        request: Req,
    ) -> <<Req as Request>::Response as ResponseType>::Type {
        for _ in 0..Self::TRIES {
            let result = self.api.send(request.clone()).await;
            match result {
                Ok(result) => {
                    return result;
                }
                Err(err) => {
                    log::error!("Error sending message: {}", err);
                    tokio::time::sleep(Duration::from_secs(1)).await;
                }
            }
        }
        panic!("Retries limit reached");
    }
}

#[derive(Copy, Clone)]
pub enum KeyboardOptions {
    None,
    Remove,
    Plus,
    YesNoPause,
    YesNoContinue,
}

impl KeyboardOptions {
    const PLUS: [&'static str; 1] = ["+"];
    const YES_NO_PAUSE: [&'static str; 3] = ["да", "нет", "пауза"];
    const YES_NO_CONTINUE: [&'static str; 3] = ["да", "нет", "продолжить"];

    pub fn reply_markup(&self) -> Option<ReplyMarkup> {
        match self {
            KeyboardOptions::None => None,
            KeyboardOptions::Remove => {
                Some(ReplyMarkup::ReplyKeyboardRemove(ReplyKeyboardRemove::new()))
            }
            KeyboardOptions::Plus => Some(Self::build_keyboard(&Self::PLUS)),
            KeyboardOptions::YesNoPause => Some(Self::build_keyboard(&Self::YES_NO_PAUSE)),
            KeyboardOptions::YesNoContinue => Some(Self::build_keyboard(&Self::YES_NO_CONTINUE)),
        }
    }

    fn build_keyboard(keys: &[&'static str]) -> ReplyMarkup {
        ReplyMarkup::ReplyKeyboardMarkup(ReplyKeyboardMarkup::from(vec![keys
            .iter()
            .map(|key| KeyboardButton::new(key))
            .collect()]))
    }
}

pub async fn download_content(url: String) -> Option<String> {
    let response = reqwest::get(url.as_str()).await;
    match response {
        Ok(response) => match response.text().await {
            Ok(text) => Some(text),
            Err(err) => {
                log::error!("Error processing downloaded file: {}", err);
                None
            }
        },
        Err(err) => {
            log::error!("Error downloading file: {}", err);
            None
        }
    }
}