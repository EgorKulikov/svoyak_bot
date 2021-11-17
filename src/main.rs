mod bot;
mod data;
mod game;
mod parser;
mod queue;
mod topic;

use crate::bot::{KeyboardOptions, TelegramBot};
use crate::data::{display_name, display_rating, BitSet, Data, UserBanResult, UserData};
use crate::game::{Game, GameHandle};
use crate::parser::parse;
use crate::queue::{PlayQueue, UpdateMessage};
use borsh::maybestd::collections::HashMap;
use env_logger::WriteStyle;
use futures::stream::select_all;
use futures::StreamExt;
use log::LevelFilter;
use std::collections::HashSet;
use std::env;
use std::time::Duration;
use telegram_bot::{ChatId, Message, MessageChat, MessageKind, UserId};
use tokio::sync::mpsc::{unbounded_channel, UnboundedSender};
use tokio::task::JoinHandle;
use tokio_stream::wrappers::UnboundedReceiverStream;

pub fn player_list(users: &[&UserData]) -> String {
    let mut res = String::new();
    for user in users {
        if !res.is_empty() {
            res += ", ";
        }
        res += format!("{} ({})", user.display_name, display_rating(user.rating)).as_str();
    }
    res
}

#[derive(Debug)]
pub enum UpdateType {
    StatusUpdate(String),
    GameEnded,
}

#[derive(Debug)]
pub struct StatusUpdate {
    pub chat_id: i64,
    pub update_type: UpdateType,
}

enum Event {
    FromScheduler(Message),
    FromPlay(Message),
    GameDataTimeout(ChatId, u32),
    GameStatus(StatusUpdate),
    QueueGame((GameStartData, String, Vec<usize>)),
}

#[derive(Debug)]
pub struct GameStartData {
    chat_ids: Vec<ChatId>,
    set_id: Option<String>,
    topic_count: u8,
    players: HashMap<UserId, UserData>,
    spectators: HashMap<UserId, UserData>,
}

struct GameData {
    chat_id: ChatId,
    set_id: Option<String>,
    topic_count: u8,
    min_players: u8,
    max_players: u8,
    players: HashMap<UserId, UserData>,
    spectators: HashMap<UserId, UserData>,
    update_id: u32,
    sender: UnboundedSender<Event>,
    expire_timer: Option<JoinHandle<()>>,
    data: Data,
}

impl GameData {
    const TIMEOUT: Duration = Duration::from_secs(300);

    pub fn new(sender: UnboundedSender<Event>, chat_id: ChatId, data: Data) -> Self {
        let mut res = Self {
            chat_id,
            set_id: None,
            topic_count: 6,
            min_players: 3,
            max_players: 4,
            players: HashMap::new(),
            spectators: HashMap::new(),
            update_id: 0u32,
            sender,
            expire_timer: None,
            data,
        };
        res.schedule_expiration();
        res
    }

    fn cancel_timer(&mut self) {
        if let Some(handle) = self.expire_timer.take() {
            handle.abort();
        }
    }

    fn schedule_expiration(&mut self) {
        self.cancel_timer();
        let sender = self.sender.clone();
        self.update_id += 1;
        let update_id = self.update_id;
        let chat_id = self.chat_id;
        self.expire_timer = Some(tokio::spawn(async move {
            tokio::time::sleep(Self::TIMEOUT).await;
            match sender.send(Event::GameDataTimeout(chat_id, update_id)) {
                Ok(_) => {}
                Err(err) => log::error!("Error with sending update: {}", err),
            }
        }));
    }

    //noinspection RsSelfConvention
    pub fn to_string(&mut self) -> String {
        for (user_id, user_data) in self.players.iter_mut() {
            *user_data = self.data.update_player(*user_id, user_data.clone());
        }
        for (user_id, user_data) in self.spectators.iter_mut() {
            *user_data = self.data.update_player(*user_id, user_data.clone());
        }
        format!(
            "{}\nТем - {}\nИгроков - {}-{}\nИгроки: {}\nЗрители: {}",
            if let Some(id) = &self.set_id {
                format!("Игра по пакету {}", id)
            } else {
                "Стандартная игра".to_string()
            },
            self.topic_count,
            self.min_players,
            self.max_players,
            player_list(&self.players.values().collect::<Vec<_>>()),
            player_list(&self.spectators.values().collect::<Vec<_>>()),
        )
    }

    pub fn set_set_id(&mut self, set_id: String) {
        self.set_id = Some(set_id);
        self.schedule_expiration();
    }

    pub fn set_topic_count(&mut self, topic_count: u8) {
        self.topic_count = topic_count;
        self.schedule_expiration();
    }

    pub fn set_min_players(&mut self, min_players: u8) {
        self.min_players = min_players;
        self.schedule_expiration();
    }

    pub fn set_max_players(&mut self, max_players: u8) {
        self.max_players = max_players;
        self.schedule_expiration();
    }

    pub fn add_player(&mut self, user_id: UserId, user_data: UserData) {
        self.spectators.remove(&user_id);
        self.players.insert(user_id, user_data);
        self.schedule_expiration();
    }

    pub fn add_spectator(&mut self, user_id: UserId, user_data: UserData) {
        self.players.remove(&user_id);
        self.spectators.insert(user_id, user_data);
        self.schedule_expiration();
    }

    pub fn remove(&mut self, user_id: UserId) {
        self.players.remove(&user_id);
        self.spectators.remove(&user_id);
        self.schedule_expiration();
    }

    pub fn to_data(&self) -> GameStartData {
        GameStartData {
            chat_ids: vec![self.chat_id],
            set_id: self.set_id.clone(),
            topic_count: self.topic_count,
            players: self.players.clone(),
            spectators: self.spectators.clone(),
        }
    }
}

struct Main {
    data: Data,
    scheduler_bot: TelegramBot,
    play_bot: TelegramBot,
    scheduler_stream: Option<UnboundedReceiverStream<Message>>,
    play_stream: Option<UnboundedReceiverStream<Message>>,
    status_sender: UnboundedSender<StatusUpdate>,
    status_receiver: Option<UnboundedReceiverStream<StatusUpdate>>,
    timeout_sender: UnboundedSender<Event>,
    timeout_receiver: Option<UnboundedReceiverStream<Event>>,
    queue: Option<PlayQueue>,
    queue_sender: UnboundedSender<UpdateMessage>,
    queue_stream: Option<UnboundedReceiverStream<(GameStartData, String, Vec<usize>)>>,
    play_chats: HashSet<ChatId>,
    games: HashMap<ChatId, (UnboundedSender<Message>, String)>,
    game_proposals: HashMap<ChatId, GameData>,
    shutting_down: bool,
}

impl Main {
    const DUMMY: i64 = 412313351i64;
    const MANAGER: i64 = 80788292i64;
    // const MAIN_CHAT: i64 = -741754684i64;
    const MAIN_CHAT: i64 = -1001053502877i64;

    pub fn new() -> Self {
        let data = Data::new("svoyak.db");
        let (scheduler_bot, scheduler_stream) =
            TelegramBot::new(env::var("SCHEDULER_BOT_TOKEN").unwrap());
        let (play_bot, play_stream) = TelegramBot::new(env::var("PLAY_BOT_TOKEN").unwrap());
        let (status_sender, status_receiver) = unbounded_channel();
        let (timeout_sender, timeout_receiver) = unbounded_channel();
        let (queue_sender, queue_receiver) = unbounded_channel();
        let (queue, queue_stream) = PlayQueue::new(
            data.clone(),
            scheduler_bot.clone(),
            UnboundedReceiverStream::new(queue_receiver),
        );

        Self {
            data,
            scheduler_bot,
            play_bot,
            scheduler_stream: Some(scheduler_stream),
            play_stream: Some(play_stream),
            status_sender,
            status_receiver: Some(UnboundedReceiverStream::new(status_receiver)),
            timeout_sender,
            timeout_receiver: Some(UnboundedReceiverStream::new(timeout_receiver)),
            queue: Some(queue),
            queue_sender,
            queue_stream: Some(queue_stream),
            play_chats: HashSet::new(),
            games: HashMap::new(),
            game_proposals: HashMap::new(),
            shutting_down: false,
        }
    }

    pub async fn run(mut self) {
        self.play_chats = self.data.get_game_chats().iter().map(|id| *id).collect();
        for game in self.data.get_game_states() {
            self.start_game(game);
        }
        let mut queue = self.queue.take().unwrap();
        tokio::spawn(async move {
            queue.start().await;
        });
        let mut stream = select_all(vec![
            self.scheduler_stream
                .take()
                .unwrap()
                .map(Event::FromScheduler)
                .boxed(),
            self.play_stream
                .take()
                .unwrap()
                .map(Event::FromPlay)
                .boxed(),
            self.status_receiver
                .take()
                .unwrap()
                .map(Event::GameStatus)
                .boxed(),
            self.timeout_receiver.take().unwrap().boxed(),
            self.queue_stream
                .take()
                .unwrap()
                .map(Event::QueueGame)
                .boxed(),
        ]);
        while let Some(event) = stream.next().await {
            match event {
                Event::FromScheduler(message) => {
                    self.process_scheduler_message(message).await;
                }
                Event::FromPlay(message) => {
                    self.process_play_message(message).await;
                }
                Event::GameDataTimeout(chat_id, update_id) => {
                    self.process_game_data_timeout(&chat_id, update_id)
                }
                Event::GameStatus(update) => {
                    self.process_status_update(update).await;
                }
                Event::QueueGame((game_start_data, set_id, topics)) => {
                    self.start_game_with_topics(&game_start_data, set_id, topics, true)
                        .await;
                }
            }
            if self.shutting_down && self.games.is_empty() {
                break;
            }
        }
        self.scheduler_bot
            .send_message(
                ChatId::new(Self::MANAGER),
                "Бот выключен".to_string(),
                KeyboardOptions::Remove,
            )
            .await;
    }

    async fn process_status_update(&mut self, update: StatusUpdate) {
        match update.update_type {
            UpdateType::StatusUpdate(status) => {
                self.games.get_mut(&ChatId::new(update.chat_id)).unwrap().1 = status;
            }
            UpdateType::GameEnded => {
                self.games.remove(&ChatId::new(update.chat_id));
            }
        }
    }

    async fn process_manager_message(&mut self, message: &Message) -> bool {
        let chat_id = message.chat.id();
        match &message.kind {
            MessageKind::Text { data, .. } => {
                let text = data.trim();
                if text.is_empty() {
                    return false;
                }
                let tokens = text.split(" ").collect::<Vec<_>>();
                let command_str = tokens[0].to_lowercase();
                let mut command = command_str.as_str();
                if let Some(pos) = command.find("@") {
                    command = &command[0..pos];
                }
                if command.starts_with("/") {
                    command = &command[1..];
                }
                let tokens = &tokens[1..];
                match command {
                    "выключение" => {
                        self.shutting_down = true;
                        self.send_shutting_down(ChatId::new(Self::MAIN_CHAT));
                        self.queue_sender.send(UpdateMessage::Shutdown).unwrap();
                        true
                    }
                    "включить" => {
                        if tokens.is_empty() {
                            self.scheduler_bot
                                .try_send_message(chat_id, "Пакет не указан".to_string());
                        } else {
                            match self.data.get_set(&tokens[0].to_string()) {
                                None => {
                                    self.scheduler_bot.try_send_message(
                                        chat_id,
                                        format!("Неизвестный пакет - {}", tokens[0]),
                                    );
                                }
                                Some(_) => {
                                    if self.data.is_active(&tokens[0].to_string()) {
                                        self.scheduler_bot.try_send_message(
                                            chat_id,
                                            "Пакет уже включен".to_string(),
                                        );
                                    } else {
                                        self.data.add_active(&tokens[0].to_string());
                                        self.scheduler_bot.try_send_message(
                                            chat_id,
                                            format!("Пакет включен - {}", tokens[0]),
                                        );
                                    }
                                }
                            }
                        }
                        true
                    }
                    "выключить" => {
                        if tokens.is_empty() {
                            self.scheduler_bot
                                .try_send_message(chat_id, "Пакет не указан".to_string());
                        } else {
                            match self.data.get_set(&tokens[0].to_string()) {
                                None => {
                                    self.scheduler_bot.try_send_message(
                                        chat_id,
                                        format!("Неизвестный пакет - {}", tokens[0]),
                                    );
                                }
                                Some(_) => {
                                    if !self.data.is_active(&tokens[0].to_string()) {
                                        self.scheduler_bot.try_send_message(
                                            chat_id,
                                            "Пакет уже выключен".to_string(),
                                        );
                                    } else {
                                        self.data.remove_active(&tokens[0].to_string());
                                        self.scheduler_bot.try_send_message(
                                            chat_id,
                                            format!("Пакет выключен - {}", tokens[0]),
                                        );
                                    }
                                }
                            }
                        }
                        true
                    }
                    "темы" => {
                        if tokens.is_empty() {
                            self.scheduler_bot
                                .try_send_message(chat_id, "Пакет не указан".to_string());
                        } else {
                            match self.data.get_set(&tokens[0].to_string()) {
                                None => {
                                    self.scheduler_bot.try_send_message(
                                        chat_id,
                                        format!("Неизвестный пакет - {}", tokens[0]),
                                    );
                                }
                                Some(set) => {
                                    let mut list = "<b>Список тем:</b>".to_string();
                                    for (i, topic) in set.topics.iter().enumerate() {
                                        list +=
                                            format!("\n<b>{}.</b> {}", i + 1, topic.name).as_str();
                                    }
                                    self.scheduler_bot.try_send_message(chat_id, list);
                                }
                            }
                        }
                        true
                    }
                    _ => false,
                }
            }
            MessageKind::Document { data, .. } => {
                match self.scheduler_bot.get_file(data.clone()).await {
                    None => {
                        self.scheduler_bot
                            .try_send_message(chat_id, "Не удалось скачать".to_string());
                    }
                    Some((id, content)) => match parse(id, content) {
                        None => {
                            self.scheduler_bot
                                .try_send_message(chat_id, "Не удалось распарсить".to_string());
                        }
                        Some(set) => {
                            let id = set.id.clone();
                            if self.data.add_new_set(&id, set) {
                                self.scheduler_bot
                                    .try_send_message(chat_id, "Пакет загружен".to_string());
                            } else {
                                self.scheduler_bot.try_send_message(
                                    chat_id,
                                    "Пакет уже был активным с другим числом тем".to_string(),
                                );
                            }
                        }
                    },
                }
                true
            }
            _ => false,
        }
    }

    async fn process_private_message(&mut self, message: Message) {
        if message.from.id == UserId::new(Self::MANAGER) {
            if self.process_manager_message(&message).await {
                return;
            }
        }
        match &message.kind {
            MessageKind::Text { data, .. } => {
                let text = data.trim();
                if text.is_empty() {
                    return;
                }
                let user_id = message.from.id;
                let tokens = text.split(" ").collect::<Vec<_>>();
                let command_str = tokens[0].to_lowercase();
                let mut command = command_str.as_str();
                if let Some(pos) = command.find("@") {
                    command = &command[0..pos];
                }
                if command.starts_with("/") {
                    command = &command[1..];
                }
                let tokens = &tokens[1..];
                match command {
                    "help" | "помощь" | "start" => {
                        self.scheduler_bot.try_send_message(user_id.into(), "Бот для спортивной своей игры. Команды:\n\
                            /help - выводит это сообщение\n\
                            /register - добавляет в очередь на создание игры\n\
                            /unregister - удаляет из очереди на создание игры\n\
                            /list - выводит список пакетов\n\
                            /status - выводит список идущих игр\n\
                            /rating - выводит таблицу рейтинга\n\
                            /block - блокирует пакет\n\
                            /unblock - разблокирует пакет. Невозможно для пакетов, заблокированных в старой версии бота.\n\
                            /played - список игроков, с которыми вы играли в последнее время\n\
                            /banlist - список игроков, которых вы заблокировали\n\
                            /ban - заблокировать игрока по номеру в списке игроков, с которыми вы играли в последнее время\n\
                            /unban - разблокировать игрока по номеру в списке игроков, которых вы заблокировали\n\
                            \n\
                            Во время игры:\n\
                            \"+\" - Если вы хотите ответить на вопрос\n\
                            \"Да\" - если вы хотите подтвердить правильность СОБСТВЕННОГО ответа, не зачтенного автоматически. Не жмите \"да\" на чужие ответы.\n\
                            \"Нет\" - если вы по ошибке нажали \"Да\" и вам засчитали неправильный ответ.\n\
                            \"Пауза\" - приостановить игру\n\
                            \"Продолжить\" - продолжить игру.\n\
                            В режиме паузы можно исправить неверно посчитанные очки. Для этого следует ввести команду \n\
                            \"Исправить\" с параметром \"количество очков\"\n\
                            Например, если вы не успели на вопрос за 50 нажать \"Да\", то следует исправить 100 очков командой: Исправить 100\n\
                            В случае необходимости вычесть очки, просто поставьте минус перед параметром: Исправить -100".to_string());
                    }
                    "register" | "+" => {
                        self.data
                            .update_player(user_id, self.data.get_or_create_user(message.from));
                        self.queue_sender
                            .send(UpdateMessage::UserEntered(user_id))
                            .unwrap();
                    }
                    "unregister" | "-" => {
                        self.queue_sender
                            .send(UpdateMessage::UserLeft(user_id))
                            .unwrap();
                    }
                    "list" | "список" => {
                        self.set_list(user_id.into());
                    }
                    "status" | "статус" => {
                        self.status(user_id.into(), None);
                    }
                    "rating" | "рейтинг" => {
                        self.rating(user_id.into(), tokens);
                    }
                    "block" => {
                        self.block_set(&message, user_id.into(), user_id, tokens);
                    }
                    "unblock" => {
                        self.unblock_set(&message, user_id.into(), user_id, tokens);
                    }
                    "played" => {
                        let played_with = self.data.get_last_played(user_id);
                        if played_with.is_empty() {
                            self.scheduler_bot.try_send_message(
                                user_id.into(),
                                "Вы ни с кем не играли".to_string(),
                            );
                        } else {
                            let mut message = "Вы играли с:".to_string();
                            for (i, id) in played_with.iter().rev().enumerate() {
                                message += format!(
                                    "\n<b>{}</b>. {}",
                                    i + 1,
                                    self.data.get_user_data(id).unwrap().display_name
                                )
                                .as_str();
                            }
                            self.scheduler_bot.try_send_message(user_id.into(), message);
                        }
                    }
                    "banlist" => {
                        let banned = self.data.get_ban_list(user_id);
                        if banned.is_empty() {
                            self.scheduler_bot.try_send_message(
                                user_id.into(),
                                "Список заблокированных пуст".to_string(),
                            );
                        } else {
                            let mut message = "Вы заблокировали:".to_string();
                            for (i, id) in banned.iter().enumerate() {
                                message += format!(
                                    "\n<b>{}</b>. {}",
                                    i + 1,
                                    self.data.get_user_data(id).unwrap().display_name
                                )
                                .as_str();
                            }
                            self.scheduler_bot.try_send_message(user_id.into(), message);
                        }
                    }
                    "ban" => {
                        if tokens.is_empty() {
                            self.scheduler_bot.try_send_message(
                                user_id.into(),
                                "Укажите номер в списке игроков, с которыми вы недавно играли"
                                    .to_string(),
                            );
                        } else {
                            match tokens[0].parse::<usize>() {
                                Err(_) => {
                                    self.scheduler_bot.try_send_message(
                                        user_id.into(),
                                        format!("Некорректное число - {}", tokens[0]),
                                    );
                                }
                                Ok(number) => {
                                    let played_with = self.data.get_last_played(user_id);
                                    if number < 1 || number > played_with.len() {
                                        self.scheduler_bot.try_send_message(
                                            user_id.into(),
                                            format!("Некорректное число - {}", tokens[0]),
                                        );
                                    } else {
                                        let to_ban = played_with[played_with.len() - number];
                                        match self.data.add_to_ban_list(user_id, to_ban) {
                                            UserBanResult::Banned => {
                                                self.scheduler_bot.try_send_message(
                                                    user_id.into(),
                                                    format!(
                                                        "Пользователь {} заблокирован",
                                                        self.data
                                                            .get_user_data(&to_ban)
                                                            .unwrap()
                                                            .display_name
                                                    ),
                                                );
                                            }
                                            UserBanResult::AlreadyInList => {
                                                self.scheduler_bot.try_send_message(
                                                    user_id.into(),
                                                    format!(
                                                        "Пользователь {} уже находится в вашем бан-листе",
                                                        self.data
                                                            .get_user_data(&to_ban)
                                                            .unwrap()
                                                            .display_name
                                                    ),
                                                );
                                            }
                                            UserBanResult::SizeLimitReached => {
                                                self.scheduler_bot.try_send_message(
                                                    user_id.into(),
                                                    "Вы достигли лимита на размер бан-листа"
                                                        .to_string(),
                                                );
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                    "unban" => {
                        if tokens.is_empty() {
                            self.scheduler_bot.try_send_message(
                                user_id.into(),
                                "Укажите номер в списке игроков, которых вы заблокировали"
                                    .to_string(),
                            );
                        } else {
                            match tokens[0].parse::<usize>() {
                                Err(_) => {
                                    self.scheduler_bot.try_send_message(
                                        user_id.into(),
                                        format!("Некорректное число - {}", tokens[0]),
                                    );
                                }
                                Ok(number) => {
                                    let banned = self.data.get_ban_list(user_id);
                                    if number < 1 || number > banned.len() {
                                        self.scheduler_bot.try_send_message(
                                            user_id.into(),
                                            format!("Некорректное число - {}", tokens[0]),
                                        );
                                    } else {
                                        let to_ban = banned[number - 1];
                                        assert!(self.data.remove_from_ban_list(user_id, to_ban));
                                        self.scheduler_bot.try_send_message(
                                            user_id.into(),
                                            format!(
                                                "Пользователь {} разблокирован",
                                                self.data
                                                    .get_user_data(&to_ban)
                                                    .unwrap()
                                                    .display_name
                                            ),
                                        );
                                    }
                                }
                            }
                        }
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }

    fn send_shutting_down(&self, chat_id: ChatId) {
        self.scheduler_bot.try_send_message(
            chat_id,
            "Бот в ближайшее время будет перезагружен. Создание новых игр временно отключено."
                .to_string(),
        );
    }

    async fn process_group_message(&mut self, message: Message) {
        match &message.kind {
            MessageKind::Text { data, .. } => {
                let chat_id = message.chat.id();
                if chat_id == ChatId::new(Self::MAIN_CHAT) {
                    return;
                }
                let text = data.trim();
                if text.is_empty() {
                    return;
                }
                let user_id = message.from.id;
                let tokens = text.split(" ").collect::<Vec<_>>();
                let command_str = tokens[0].to_lowercase();
                let mut command = command_str.as_str();
                if let Some(pos) = command.find("@") {
                    command = &command[0..pos];
                }
                if command.starts_with("/") {
                    command = &command[1..];
                }
                let tokens = &tokens[1..];
                let game_data = self.game_proposals.get(&chat_id);
                match command {
                    "help" | "помощь" => {
                        self.scheduler_bot.try_send_message(chat_id, "Бот для спортивной своей игры. Команды:\n\
                            /help - выводит это сообщение\n\
                            /game - создает новую игру\n\
                            /set - задает пакет, на которм будет идти игра\n\
                            /topics - устанавливает число тем\n\
                            /minplayers - устанавливает минимальное число игроков\n\
                            /maxplayers - устанавливает максимальное число игроков\n\
                            /register - регистрирует на текущую игру и создает игру, если она не начата\n\
                            /spectator - регистрирует на текущую игру зрителем\n\
                            /unregister - отменяет регистрацию\n\
                            /start - стартует текущую игру\n\
                            /abort - отменяет текущую игру\n\
                            /list - выводит список пакетов\n\
                            /status - выводит список идущих игр\n\
                            /rating - выводит таблицу рейтинга\n\
                            /block - блокирует пакет\n\
                            /unblock - разблокирует пакет. Невозможно для пакетов, заблокированных в старой версии бота.\n\
                            \n\
                            Во время игры:\n\
                            \"+\" - Если вы хотите ответить на вопрос\n\
                            \"Да\" - если вы хотите подтвердить правильность СОБСТВЕННОГО ответа, не зачтенного автоматически. Не жмите \"да\" на чужие ответы.\n\
                            \"Нет\" - если вы по ошибке нажали \"Да\" и вам засчитали неправильный ответ.\n\
                            \"Пауза\" - приостановить игру\n\
                            \"Продолжить\" - продолжить игру.\n\
                            В режиме паузы можно исправить неверно посчитанные очки. Для этого следует ввести команду \n\
                            \"Исправить\" с параметром \"количество очков\"\n\
                            Например, если вы не успели на вопрос за 50 нажать \"Да\", то следует исправить 100 очков командой: Исправить 100\n\
                            В случае необходимости вычесть очки, просто поставьте минус перед параметром: Исправить -100".to_string());
                    }
                    "game" | "игра" => {
                        if self.shutting_down {
                            self.send_shutting_down(chat_id);
                            return;
                        }
                        match game_data {
                            None => {
                                let mut game_data = GameData::new(
                                    self.timeout_sender.clone(),
                                    chat_id,
                                    self.data.clone(),
                                );
                                self.scheduler_bot
                                    .try_send_message(chat_id, game_data.to_string());
                                self.game_proposals.insert(chat_id, game_data);
                            }
                            Some(_) => {
                                self.scheduler_bot.try_send_message(
                                    chat_id,
                                    "Существует активная игра".to_string(),
                                );
                            }
                        }
                    }
                    "set" | "пакет" => match game_data {
                        None => {
                            self.scheduler_bot
                                .try_send_message(chat_id, "Игра не начата".to_string());
                        }
                        Some(_) => {
                            if tokens.is_empty() {
                                self.scheduler_bot
                                    .try_send_message(chat_id, "Укажите пакет".to_string());
                            } else {
                                if self.data.is_active(&tokens[0].to_string()) {
                                    let game_data = self.game_proposals.get_mut(&chat_id).unwrap();
                                    game_data.set_set_id(tokens[0].to_string());
                                    self.scheduler_bot
                                        .try_send_message(chat_id, game_data.to_string());
                                } else {
                                    self.scheduler_bot.try_send_message(
                                        chat_id,
                                        format!("Пакет не обнаружен - {}", tokens[0]),
                                    );
                                }
                            }
                        }
                    },
                    "topics" | "темы" => match game_data {
                        None => {
                            self.scheduler_bot
                                .try_send_message(chat_id, "Игра не начата".to_string());
                        }
                        Some(_) => {
                            if tokens.is_empty() {
                                self.scheduler_bot
                                    .try_send_message(chat_id, "Укажите число".to_string());
                            } else {
                                match tokens[0].parse::<u8>() {
                                    Err(_) => {
                                        self.scheduler_bot.try_send_message(
                                            chat_id,
                                            format!("Некорректное число - {}", tokens[0]),
                                        );
                                    }
                                    Ok(number) => {
                                        if number < 1 || number > 20 {
                                            self.scheduler_bot.try_send_message(
                                                chat_id,
                                                format!("Некорректное число - {}", tokens[0]),
                                            );
                                        } else {
                                            let game_data =
                                                self.game_proposals.get_mut(&chat_id).unwrap();
                                            game_data.set_topic_count(number);
                                            self.scheduler_bot
                                                .try_send_message(chat_id, game_data.to_string());
                                        }
                                    }
                                }
                            }
                        }
                    },
                    "minplayers" | "минигроков" => match game_data {
                        None => {
                            self.scheduler_bot
                                .try_send_message(chat_id, "Игра не начата".to_string());
                        }
                        Some(game_data) => {
                            if tokens.is_empty() {
                                self.scheduler_bot
                                    .try_send_message(chat_id, "Укажите число".to_string());
                            } else {
                                match tokens[0].parse::<u8>() {
                                    Err(_) => {
                                        self.scheduler_bot.try_send_message(
                                            chat_id,
                                            format!("Некорректное число - {}", tokens[0]),
                                        );
                                    }
                                    Ok(number) => {
                                        if number < 1 || number > game_data.max_players {
                                            self.scheduler_bot.try_send_message(
                                                chat_id,
                                                format!("Некорректное число - {}", tokens[0]),
                                            );
                                        } else {
                                            let game_data =
                                                self.game_proposals.get_mut(&chat_id).unwrap();
                                            game_data.set_min_players(number);
                                            self.scheduler_bot
                                                .try_send_message(chat_id, game_data.to_string());
                                        }
                                    }
                                }
                            }
                        }
                    },
                    "maxplayers" | "максигроков" => match game_data {
                        None => {
                            self.scheduler_bot
                                .try_send_message(chat_id, "Игра не начата".to_string());
                        }
                        Some(game_data) => {
                            if tokens.is_empty() {
                                self.scheduler_bot
                                    .try_send_message(chat_id, "Укажите число".to_string());
                            } else {
                                match tokens[0].parse::<u8>() {
                                    Err(_) => {
                                        self.scheduler_bot.try_send_message(
                                            chat_id,
                                            format!("Некорректное число - {}", tokens[0]),
                                        );
                                    }
                                    Ok(number) => {
                                        if number
                                            < game_data
                                                .min_players
                                                .max(game_data.players.len() as u8)
                                            || number > 20
                                        {
                                            self.scheduler_bot.try_send_message(
                                                chat_id,
                                                format!("Некорректное число - {}", tokens[0]),
                                            );
                                        } else {
                                            let game_data =
                                                self.game_proposals.get_mut(&chat_id).unwrap();
                                            game_data.set_max_players(number);
                                            self.scheduler_bot
                                                .try_send_message(chat_id, game_data.to_string());
                                        }
                                    }
                                }
                            }
                        }
                    },
                    "register" | "+" => {
                        if self.shutting_down {
                            self.send_shutting_down(chat_id);
                            return;
                        }
                        let game_data = match game_data {
                            None => {
                                let game_data = GameData::new(
                                    self.timeout_sender.clone(),
                                    chat_id,
                                    self.data.clone(),
                                );
                                self.game_proposals.insert(chat_id, game_data);
                                self.game_proposals.get_mut(&chat_id).unwrap()
                            }
                            Some(_) => self.game_proposals.get_mut(&chat_id).unwrap(),
                        };
                        if game_data.players.len() as u8 == game_data.max_players
                            && !game_data.players.contains_key(&user_id)
                        {
                            self.scheduler_bot
                                .try_send_message(chat_id, "Все места заняты".to_string());
                        } else {
                            game_data
                                .add_player(user_id, self.data.get_or_create_user(message.from));
                            self.scheduler_bot
                                .try_send_message(chat_id, game_data.to_string());
                        }
                    }
                    "spectator" | "зритель" => {
                        if self.shutting_down {
                            self.send_shutting_down(chat_id);
                            return;
                        }
                        match game_data {
                            None => {
                                self.scheduler_bot
                                    .try_send_message(chat_id, "Игра не начата".to_string());
                            }
                            Some(_) => {
                                let game_data = self.game_proposals.get_mut(&chat_id).unwrap();
                                game_data.add_spectator(
                                    user_id,
                                    self.data.get_or_create_user(message.from),
                                );
                                self.scheduler_bot
                                    .try_send_message(chat_id, game_data.to_string());
                            }
                        }
                    }
                    "unregister" | "-" => match game_data {
                        None => {
                            self.scheduler_bot
                                .try_send_message(chat_id, "Игра не начата".to_string());
                        }
                        Some(_) => {
                            let game_data = self.game_proposals.get_mut(&chat_id).unwrap();
                            game_data.remove(user_id);
                            self.scheduler_bot
                                .try_send_message(chat_id, game_data.to_string());
                        }
                    },
                    "abort" => match game_data {
                        None => {
                            self.scheduler_bot
                                .try_send_message(chat_id, "Игра не начата".to_string());
                        }
                        Some(_) => {
                            self.game_proposals.remove(&chat_id);
                            self.scheduler_bot
                                .try_send_message(chat_id, "Игра отменена".to_string());
                        }
                    },
                    "start" | "старт" => {
                        if self.shutting_down {
                            self.send_shutting_down(chat_id);
                            return;
                        }
                        match game_data {
                            None => {
                                self.scheduler_bot
                                    .try_send_message(chat_id, "Игра не начата".to_string());
                            }
                            Some(game_data) => {
                                assert!((game_data.players.len() as u8) <= game_data.max_players);
                                if (game_data.players.len() as u8) < game_data.min_players {
                                    self.scheduler_bot.try_send_message(
                                        chat_id,
                                        "Недостаточно игроков".to_string(),
                                    );
                                } else {
                                    let game_start_data = game_data.to_data();
                                    self.game_proposals
                                        .get_mut(&chat_id)
                                        .unwrap()
                                        .cancel_timer();
                                    self.try_start_game(game_start_data).await;
                                    self.game_proposals.remove(&chat_id);
                                }
                            }
                        }
                    }
                    "list" | "список" => {
                        self.set_list(chat_id);
                    }
                    "status" | "статус" => {
                        self.status(chat_id, game_data);
                    }
                    "rating" | "рейтинг" => {
                        self.rating(chat_id, tokens);
                    }
                    "block" => {
                        self.block_set(&message, chat_id, user_id, tokens);
                    }
                    "unblock" => {
                        self.unblock_set(&message, chat_id, user_id, tokens);
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }

    fn rating(&self, chat_id: ChatId, tokens: &[&str]) {
        let top = if tokens.is_empty() {
            20
        } else {
            match tokens[0].parse::<usize>() {
                Ok(value) => value.min(200usize),
                Err(_) => 20,
            }
        };
        let bot = self.scheduler_bot.clone();
        let data = self.data.clone();
        tokio::spawn(async move {
            bot.try_send_message(
                chat_id,
                format!("<b>Рейтинг игроков:</b>\n{}", data.get_rating_list(top)),
            )
        });
    }

    fn status(&self, chat_id: ChatId, game_data: Option<&GameData>) {
        let mut status = match game_data {
            None => "",
            Some(_) => "Открыта регистрация\n",
        }
        .to_string();
        if self.games.is_empty() {
            status += "Игр не идет";
        } else {
            for (_, (_, game_status)) in self.games.iter() {
                status += game_status.as_str();
            }
        }
        self.scheduler_bot.try_send_message(chat_id, status);
    }

    fn set_list(&self, chat_id: ChatId) {
        let list = self.data.get_active_set_ids();
        let mut message = "<b>Список пакетов:</b>\n".to_string();
        for id in list {
            message += format!(
                "<b>{}</b> - {}\n",
                id,
                self.data.get_set(&id).unwrap().title
            )
            .as_str();
        }
        self.scheduler_bot.try_send_message(chat_id, message);
    }

    fn unblock_set(&self, message: &Message, chat_id: ChatId, user_id: UserId, tokens: &[&str]) {
        if tokens.is_empty() {
            self.scheduler_bot
                .try_send_message(chat_id, "Укажите пакет".to_string());
        } else {
            if self
                .data
                .get_active_set_ids()
                .contains(&tokens[0].to_string())
            {
                if self
                    .data
                    .set_set_blocked(user_id, &tokens[0].to_string(), false)
                {
                    self.scheduler_bot.try_send_message(
                        chat_id,
                        format!(
                            "Пакет {} разблокирован для пользователя {}",
                            tokens[0],
                            display_name(&message.from)
                        ),
                    );
                } else {
                    self.scheduler_bot.try_send_message(
                        chat_id,
                        format!(
                            "Пакет {} не был заблокирован для пользователя {}",
                            tokens[0],
                            display_name(&message.from)
                        ),
                    );
                }
            } else {
                self.scheduler_bot
                    .try_send_message(chat_id, format!("Пакет не обнаружен - {}", tokens[0]));
            }
        }
    }

    fn block_set(&self, message: &Message, chat_id: ChatId, user_id: UserId, tokens: &[&str]) {
        if tokens.is_empty() {
            self.scheduler_bot
                .try_send_message(chat_id, "Укажите пакет".to_string());
        } else {
            if self
                .data
                .get_active_set_ids()
                .contains(&tokens[0].to_string())
            {
                if self
                    .data
                    .set_set_blocked(user_id, &tokens[0].to_string(), true)
                {
                    self.scheduler_bot.try_send_message(
                        chat_id,
                        format!(
                            "Пакет {} заблокирован для пользователя {}",
                            tokens[0],
                            display_name(&message.from)
                        ),
                    );
                } else {
                    self.scheduler_bot.try_send_message(
                        chat_id,
                        format!(
                            "Пакет {} уже был заблокирован для пользователя {}",
                            tokens[0],
                            display_name(&message.from)
                        ),
                    );
                }
            } else {
                self.scheduler_bot
                    .try_send_message(chat_id, format!("Пакет не обнаружен - {}", tokens[0]));
            }
        }
    }

    async fn try_start_game(&mut self, mut game_data: GameStartData) {
        match find_topics(&self.data, &mut game_data) {
            None => {
                for chat_id in game_data.chat_ids.iter() {
                    self.scheduler_bot.try_send_message(
                        *chat_id,
                        "Недостаточно тем, которые бы не играли все игроки".to_string(),
                    );
                }
            }
            Some((set_id, topics)) => {
                assert_eq!(topics.len() as u8, game_data.topic_count);
                self.start_game_with_topics(&game_data, set_id, topics, false)
                    .await;
            }
        }
    }

    async fn start_game_with_topics(
        &mut self,
        game_data: &GameStartData,
        set_id: String,
        topics: Vec<usize>,
        from_private: bool,
    ) {
        let chat_id = self
            .play_chats
            .iter()
            .filter(|chat_id| !self.games.contains_key(chat_id))
            .next();
        if chat_id.is_none() {
            for chat_id in game_data.chat_ids.iter() {
                self.scheduler_bot.try_send_message(
                    *chat_id,
                    "На текущий момент свободных комнат нет".to_string(),
                );
            }
            return;
        }
        let chat_id = chat_id.unwrap().clone();
        self.data.set_played(
            &game_data
                .players
                .keys()
                .chain(game_data.spectators.keys())
                .collect::<Vec<_>>()[..],
            &set_id,
            &topics[..],
        );
        self.data
            .add_game(&game_data.players.keys().cloned().collect::<Vec<_>>());
        let mut user_list = String::new();
        for (user_id, user_data) in game_data.players.iter() {
            if !user_list.is_empty() {
                user_list += ", ";
            }
            user_list += format!(
                "<a href=\"tg://user?id={}\">{}</a>",
                user_id, user_data.display_name
            )
            .as_str();
        }
        let invite_link = self.play_bot.create_invite_link(chat_id).await;
        for chat_id in game_data.chat_ids.iter() {
            if from_private {
                log::info!(
                    "Invite sent to {}",
                    self.data
                        .get_user_data(&UserId::new((*chat_id).into()))
                        .unwrap()
                        .display_name
                );
                self.scheduler_bot.try_send_message(
                    *chat_id,
                    format!("Игра найдена! Для игры пройдите по ссылке: {}", invite_link),
                );
            } else {
                self.scheduler_bot.try_send_message(
                    *chat_id,
                    format!(
                        "{} - для игры пройдите по ссылке: {}",
                        user_list, invite_link
                    ),
                );
            }
        }
        self.start_game(Game::new(
            chat_id.into(),
            game_data
                .chat_ids
                .iter()
                .map(|id| id.clone().into())
                .collect(),
            set_id,
            topics,
            game_data
                .players
                .iter()
                .map(|(user_id, user_data)| ((*user_id).into(), user_data.clone()))
                .collect::<HashMap<i64, UserData>>(),
            game_data
                .spectators
                .keys()
                .map(|user_id| (*user_id).into())
                .collect::<HashSet<i64>>(),
            invite_link,
        ));
    }

    async fn process_scheduler_message(&mut self, message: Message) {
        match message.chat {
            MessageChat::Private(_) => {
                self.process_private_message(message).await;
            }
            MessageChat::Group(_) | MessageChat::Supergroup(_) => {
                self.process_group_message(message).await;
            }
            MessageChat::Unknown(_) => {}
        }
    }

    async fn process_play_message(&mut self, message: Message) {
        if message.from.id == UserId::new(Self::DUMMY) {
            match message.kind {
                MessageKind::Text { data, .. } => {
                    if data == "добавить" {
                        if self.play_chats.insert(message.chat.id()) {
                            self.data.add_game_chat(&message.chat.id().into());
                            self.play_bot
                                .send_message(
                                    message.chat.id(),
                                    "Чат добавлен".to_string(),
                                    KeyboardOptions::None,
                                )
                                .await;
                        } else {
                            self.play_bot
                                .send_message(
                                    message.chat.id(),
                                    "Чат уже добавлен".to_string(),
                                    KeyboardOptions::None,
                                )
                                .await;
                        }
                    } else if data == "удалить" {
                        if self.play_chats.remove(&message.chat.id()) {
                            self.data.remove_game_chat(&message.chat.id().into());
                            self.play_bot
                                .send_message(
                                    message.chat.id(),
                                    "Чат удален".to_string(),
                                    KeyboardOptions::None,
                                )
                                .await;
                        } else {
                            self.play_bot
                                .send_message(
                                    message.chat.id(),
                                    "Чат не в списке".to_string(),
                                    KeyboardOptions::None,
                                )
                                .await;
                        }
                    }
                }
                _ => {}
            }
        } else {
            match &message.chat {
                MessageChat::Private(_) => {
                    self.play_bot.try_send_message(
                        message.chat.id(),
                        "Если вы хотите играть в свою игру, добавьте @SvoyakSchedulerBot"
                            .to_string(),
                    );
                }
                MessageChat::Group(_) => {
                    if self.play_chats.contains(&message.chat.id()) {
                        if let Some((sender, _)) = self.games.get(&message.chat.id()) {
                            sender.send(message).unwrap();
                        } else {
                            match message.kind {
                                MessageKind::Text { .. } => {
                                    self.play_bot.kick(message.chat.id(), message.from.id).await;
                                }
                                MessageKind::NewChatMembers { data } => {
                                    for user in data {
                                        self.play_bot.kick(message.chat.id(), user.id).await;
                                    }
                                }
                                _ => {}
                            }
                        }
                    } else {
                        self.play_bot.try_send_message(
                            message.chat.id(),
                            "Если вы хотите играть в свою игру, добавьте @SvoyakSchedulerBot"
                                .to_string(),
                        );
                    }
                }
                MessageChat::Supergroup(_) => {}
                MessageChat::Unknown(_) => {}
            }
        }
    }

    fn process_game_data_timeout(&mut self, chat_id: &ChatId, update_id: u32) {
        if let Some(data) = self.game_proposals.get(&chat_id) {
            if data.update_id == update_id {
                self.game_proposals.remove(&chat_id);
                self.scheduler_bot.try_send_message(
                    chat_id.clone(),
                    "Игра отменена из-за отсутствия активности".to_string(),
                );
            }
        }
    }

    fn start_game(&mut self, game: Game) {
        log::info!("Игра начата: {:#?}", game);
        let chat_id = game.chat_id;
        let set_id = game.set_id.clone();
        let handle = GameHandle::create_game(
            self.play_bot.clone(),
            self.scheduler_bot.clone(),
            self.status_sender.clone(),
            game,
            self.data.get_set(&set_id).unwrap(),
            self.data.clone(),
        );
        let (game_sender, game_receiver) = unbounded_channel();
        self.games
            .insert(ChatId::new(chat_id), (game_sender, handle.status()));
        tokio::spawn(async move {
            handle
                .start_game(UnboundedReceiverStream::new(game_receiver))
                .await;
        });
    }
}

pub fn find_topics(data: &Data, game_data: &mut GameStartData) -> Option<(String, Vec<usize>)> {
    let set_ids = match game_data.set_id.take() {
        None => data.get_active_set_ids(),
        Some(set_id) => vec![set_id],
    };
    for set_id in set_ids {
        let mut good = true;
        for user_id in game_data.players.keys() {
            if data.topics_in_set_remain(*user_id, &set_id) < game_data.topic_count as usize {
                good = false;
                break;
            }
        }
        if !good {
            continue;
        }
        let set = data.get_set(&set_id).unwrap();
        let total = set.topics.len();
        let mut unused = BitSet::new(total);
        for user_id in game_data.players.keys() {
            match data.get_played(*user_id, &set_id) {
                None => {}
                Some(bit_set) => {
                    unused.unite(&bit_set);
                }
            }
            if unused.size + (game_data.topic_count as usize) > total {
                good = false;
                break;
            }
        }
        if good {
            let mut topics = Vec::new();
            for i in 0..total {
                if !unused.is_set(i) {
                    topics.push(i);
                    if topics.len() as u8 == game_data.topic_count {
                        break;
                    }
                }
            }
            return Some((set_id, topics));
        }
    }
    None
}

async fn async_main() {
    let main = Main::new();
    main.run().await;
}

fn main() {
    env_logger::Builder::new()
        .filter(None, LevelFilter::Warn)
        .format_timestamp_millis()
        .write_style(WriteStyle::Always)
        .init();
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async_main());
}
