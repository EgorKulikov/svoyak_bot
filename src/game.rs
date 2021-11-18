use crate::bot::{KeyboardOptions, TelegramBot};
use crate::data::{display_rating, Data, UserData};
use crate::topic::{encode, Question, Topic, TopicSet};
use crate::{player_list, StatusUpdate, UpdateType};
use borsh::{BorshDeserialize, BorshSerialize};
use futures::{stream::select_all, StreamExt};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;
use telegram_bot::{ChatId, Message, MessageId, MessageKind, UserId};
use tokio::sync::mpsc::{unbounded_channel, UnboundedSender};
use tokio::task::JoinHandle;
use tokio_stream::wrappers::UnboundedReceiverStream;

#[derive(BorshSerialize, BorshDeserialize, Clone, PartialEq, Debug)]
pub enum GameState {
    BeforeGame(bool, u8),
    BeforeTopic(bool),
    BeforeFirstQuestion(bool),
    BeforeQuestion(bool),
    Question(i64, Vec<i64>),
    Answer(i64, Vec<i64>, i64),
    AfterQuestion(bool, Vec<i64>, Option<i64>),
    SpecialScore(bool),
    AfterGame,
}

impl GameState {
    pub fn before(&self) -> bool {
        match self {
            GameState::BeforeGame(_, _) => true,
            _ => false,
        }
    }

    pub fn keyboard_options(&self) -> KeyboardOptions {
        match self {
            GameState::BeforeGame(_, _) => KeyboardOptions::Remove,
            GameState::BeforeTopic(_) => KeyboardOptions::Remove,
            GameState::BeforeFirstQuestion(_) => KeyboardOptions::Remove,
            GameState::BeforeQuestion(_) => KeyboardOptions::Plus,
            GameState::Question(_, _) => KeyboardOptions::None,
            GameState::Answer(_, _, _) => KeyboardOptions::Remove,
            GameState::AfterQuestion(false, _, _) => KeyboardOptions::YesNoPause,
            GameState::AfterQuestion(true, _, _) => KeyboardOptions::YesNoContinue,
            GameState::SpecialScore(_) => KeyboardOptions::Remove,
            GameState::AfterGame => KeyboardOptions::Remove,
        }
    }

    pub fn set_pause(&mut self, to_pause: bool) -> bool {
        match self {
            GameState::BeforeGame(paused, _)
            | GameState::BeforeTopic(paused)
            | GameState::BeforeFirstQuestion(paused)
            | GameState::BeforeQuestion(paused)
            | GameState::AfterQuestion(paused, _, _)
            | GameState::SpecialScore(paused) => {
                let was = *paused;
                *paused = to_pause;
                was
            }
            GameState::Question(_, _) | GameState::Answer(_, _, _) | GameState::AfterGame => false,
        }
    }

    pub fn paused(&self) -> bool {
        match self {
            GameState::BeforeGame(paused, _)
            | GameState::BeforeTopic(paused)
            | GameState::BeforeFirstQuestion(paused)
            | GameState::BeforeQuestion(paused)
            | GameState::AfterQuestion(paused, _, _)
            | GameState::SpecialScore(paused) => *paused,
            GameState::Question(_, _) | GameState::Answer(_, _, _) | GameState::AfterGame => {
                unreachable!()
            }
        }
    }

    pub fn pausable(&self) -> bool {
        match self {
            GameState::BeforeGame(..)
            | GameState::BeforeTopic(..)
            | GameState::AfterQuestion(..)
            | GameState::SpecialScore(..)
            | GameState::BeforeFirstQuestion(..)
            | GameState::BeforeQuestion(..) => true,
            GameState::Question(..) | GameState::Answer(..) | GameState::AfterGame => false,
        }
    }
}

#[derive(BorshSerialize, BorshDeserialize, Debug)]
pub struct Game {
    pub chat_id: i64,
    source_chats: Vec<i64>,
    game_state: GameState,
    pub set_id: String,
    topics: Vec<usize>,
    current_topic: usize,
    current_question: usize,
    players: HashMap<i64, (UserData, i32, bool)>,
    spectators: HashSet<i64>,
    invite_link: String,
}

impl Game {
    pub fn new(
        chat_id: i64,
        source_chats: Vec<i64>,
        set_id: String,
        topics: Vec<usize>,
        players: HashMap<i64, UserData>,
        spectators: HashSet<i64>,
        invite_link: String,
    ) -> Self {
        Game {
            chat_id,
            source_chats,
            game_state: GameState::BeforeGame(false, 5u8),
            set_id,
            topics,
            current_topic: 0usize,
            current_question: 0usize,
            players: players
                .iter()
                .map(|(k, v)| (*k, (v.clone(), 0i32, false)))
                .collect(),
            spectators,
            invite_link,
        }
    }
}

pub enum Event {
    Message(Message),
    Timeout(u64),
}

pub struct GameHandle {
    timeout_sender: UnboundedSender<Event>,
    timeout_stream: Option<UnboundedReceiverStream<Event>>,
    timeout_handle: Option<JoinHandle<()>>,
    status_sender: UnboundedSender<StatusUpdate>,
    game: Game,
    play_bot: TelegramBot,
    scheduler_bot: TelegramBot,
    data: Data,
    topic_set: Arc<TopicSet>,
    state_id: u64,
}

impl GameHandle {
    const PAUSE: Duration = Duration::from_secs(600);
    const AFTER_GAME: Duration = Duration::from_secs(60);
    const INTERMISSION: Duration = Duration::from_secs(8);
    const PRE_GAME_STEP: Duration = Duration::from_secs(60);
    const FIRST_THINKING: Duration = Duration::from_secs(15);
    const SUCCESSIVE_THINKING: Duration = Duration::from_secs(10);
    const PRE_GAME: Duration = Duration::from_secs(15);
    const ANSWER: Duration = Duration::from_secs(30);

    pub fn create_game(
        play_bot: TelegramBot,
        scheduler_bot: TelegramBot,
        status_sender: UnboundedSender<StatusUpdate>,
        game: Game,
        topic_set: Arc<TopicSet>,
        data: Data,
    ) -> Self {
        let (timeout_sender, timeout_receiver) = unbounded_channel();
        let timeout_stream = UnboundedReceiverStream::new(timeout_receiver);
        GameHandle {
            timeout_sender,
            timeout_stream: Some(timeout_stream),
            timeout_handle: None,
            status_sender,
            game,
            play_bot,
            scheduler_bot,
            data,
            topic_set,
            state_id: 0u64,
        }
    }

    async fn send_message(&self, text: String) -> Option<i64> {
        self.send_message_with_markup(text, self.game.game_state.keyboard_options())
            .await
    }

    async fn send_message_with_markup(
        &self,
        text: String,
        keyboard_options: KeyboardOptions,
    ) -> Option<i64> {
        self.play_bot
            .send_message(ChatId::new(self.game.chat_id), text, keyboard_options)
            .await
            .map(|id| id.into())
    }

    fn schedule_timeout(&mut self, duration: Duration) {
        self.cancel_timer();
        self.state_id += 1;
        let sender = self.timeout_sender.clone();
        let id = self.state_id;
        self.timeout_handle = Some(tokio::spawn(async move {
            tokio::time::sleep(duration).await;
            match sender.send(Event::Timeout(id)) {
                Ok(_) => {}
                Err(err) => log::error!("Error with sending update: {}", err),
            }
        }))
    }

    async fn end_question(&mut self, pause_game: bool) {
        if let GameState::Question(_, answers) = &self.game.game_state {
            self.game.game_state = GameState::AfterQuestion(pause_game, answers.clone(), None);
            self.send_message(self.current_question().display_answers(false))
                .await;
            self.schedule_timeout(Self::INTERMISSION);
        } else {
            unreachable!()
        }
    }

    async fn incorrect_answer(&mut self, timeout: bool, force_stop_timer: bool) {
        if let GameState::Answer(message_id, answers, current) = self.game.game_state.clone() {
            let restart_timer = !force_stop_timer && answers.len() + 1 != self.game.players.len();
            let mut answers = answers.clone();
            answers.push(current);
            self.game.game_state = GameState::Question(message_id, answers);
            self.edit_message(&message_id).await;
            self.play_bot
                .send_message(
                    ChatId::new(self.game.chat_id),
                    format!(
                        "{}, {}",
                        if timeout {
                            "Время вышло"
                        } else {
                            "Это неправильный ответ"
                        },
                        self.user_name(&current),
                    ),
                    KeyboardOptions::Plus,
                )
                .await;
            if restart_timer {
                self.schedule_timeout(Self::SUCCESSIVE_THINKING);
            } else {
                match self.timeout_sender.send(Event::Timeout(self.state_id)) {
                    Ok(_) => {}
                    Err(err) => panic!("Error with sending update: {}", err),
                }
            }
        } else {
            unreachable!()
        }
    }

    async fn edit_message(&mut self, message_id: &i64) {
        self.play_bot
            .edit_message(
                ChatId::new(self.game.chat_id),
                MessageId::new(*message_id),
                self.question_text(),
            )
            .await;
    }

    async fn end_game(&mut self, aborted: bool) {
        self.game.game_state = GameState::AfterGame;
        self.send_message("Игра окончена!".to_string()).await;
        let outcome = if !aborted {
            let current_ratings = self
                .game
                .players
                .iter()
                .map(|(id, _)| {
                    (
                        *id,
                        self.data.get_user_data(&UserId::new(*id)).unwrap().rating,
                    )
                })
                .collect::<HashMap<_, _>>();
            if !aborted {
                self.data.save_game_results(&self.game.players);
            }
            let updated_ratings = self
                .game
                .players
                .iter()
                .map(|(id, _)| {
                    (
                        *id,
                        self.data.get_user_data(&UserId::new(*id)).unwrap().rating,
                    )
                })
                .collect::<HashMap<_, _>>();
            let mut entries = self
                .game
                .players
                .iter()
                .map(|(id, (user, score, _))| {
                    (
                        *score,
                        &user.display_name,
                        current_ratings[id],
                        updated_ratings[id],
                    )
                })
                .collect::<Vec<_>>();
            entries.sort_by(|(sc1, ..), (sc2, ..)| sc2.cmp(sc1));
            let mut result = String::new();
            for (score, name, old_rating, new_rating) in entries {
                result += format!(
                    "{} {} {} ({})\n",
                    name,
                    score,
                    display_rating(new_rating),
                    (display_rating(new_rating) as i64) - (display_rating(old_rating) as i64)
                )
                .as_str();
            }
            result
        } else {
            "Игра отменена".to_string()
        };
        self.schedule_timeout(Self::AFTER_GAME);
        for source_id in self.game.source_chats.iter() {
            self.scheduler_bot.try_send_message(
                ChatId::new(*source_id),
                format!(
                    "<b>Игра завершена.</b>\nПакет: {}\n{}",
                    self.topic_set.title, outcome
                ),
            );
        }
    }

    fn update_status(&self) {
        self.status_sender
            .send(StatusUpdate {
                chat_id: self.game.chat_id,
                update_type: UpdateType::StatusUpdate(self.status()),
            })
            .unwrap();
    }

    pub fn status(&self) -> String {
        format!(
            "\nИгра по пакету {}\nИгроки: {}\n{}",
            self.topic_set.title,
            player_list(
                &self
                    .game
                    .players
                    .iter()
                    .map(|(_, (player, ..))| player)
                    .collect::<Vec<_>>()[..]
            ),
            if self.game.game_state == GameState::AfterGame {
                "\nИгра окончена\n".to_string()
            } else {
                format!(
                    "Тема {}/{}\n",
                    self.game.topics.len().min(self.game.current_topic + 1),
                    self.game.topics.len()
                )
            }
        )
    }

    fn user_name(&self, id: &i64) -> &String {
        &self.game.players[id].0.display_name
    }

    async fn process_message(&mut self, message: Message) {
        match message.kind {
            MessageKind::Text { data, .. } => {
                let from = &message.from.id.into();
                let data = data.trim().to_string();
                let tokens = data.split(" ").collect::<Vec<_>>();
                if tokens.is_empty() {
                    return;
                }
                let command_str = tokens[0].to_lowercase();
                let mut command = command_str.as_str();
                if command.starts_with("/") {
                    command = &command[1..];
                }
                let tokens = &tokens[1..];
                if command == "abort" {
                    self.end_game(true).await;
                    self.data.save_game_state(&self.game);
                    return;
                } else if self.game.game_state.pausable() {
                    if (command == "pause" || command == "пауза")
                        && !self.game.game_state.set_pause(true)
                    {
                        self.send_message("Игра приостановлена".to_string()).await;
                        self.schedule_timeout(Self::PAUSE);
                        self.data.save_game_state(&self.game);
                        return;
                    } else if (command == "continue" || command == "продолжить")
                        && self.game.game_state.set_pause(false)
                    {
                        self.send_message("Игра возобновлена".to_string()).await;
                        self.schedule_timeout(Self::INTERMISSION);
                        self.data.save_game_state(&self.game);
                        return;
                    } else if self.game.game_state.paused()
                        && (command == "adjust" || command == "исправить")
                    {
                        if tokens.len() != 1 {
                            self.send_message("Неверное число аргументов".to_string())
                                .await;
                        } else {
                            match tokens[0].parse::<i32>() {
                                Ok(by) => {
                                    if by.abs() > 10000 || by % 10 != 0 {
                                        self.send_message("Некорректное число очков".to_string())
                                            .await;
                                    } else {
                                        match self.game.players.get_mut(from) {
                                            None => {}
                                            Some(player_data) => {
                                                player_data.1 += by;
                                                self.send_message(format!(
                                                    "Новое количество очков у {} - {}",
                                                    self.user_name(from),
                                                    self.game.players[from].1,
                                                ))
                                                .await;
                                            }
                                        }
                                        self.update_status();
                                    }
                                }
                                Err(_) => {
                                    self.send_message("Некорректное число очков".to_string())
                                        .await;
                                }
                            }
                        }
                        self.data.save_game_state(&self.game);
                        return;
                    }
                }
                match self.game.game_state.clone() {
                    GameState::BeforeGame(_, _) => {}
                    GameState::BeforeTopic(_) => {}
                    GameState::BeforeFirstQuestion(_) => {}
                    GameState::BeforeQuestion(_) => {}
                    GameState::Question(message_id, answers) => {
                        if command == "+"
                            && !answers.contains(from)
                            && self.game.players.contains_key(from)
                        {
                            self.game.game_state = GameState::Answer(message_id, answers, *from);
                            self.play_bot
                                .edit_message(
                                    message.chat.id(),
                                    MessageId::new(message_id),
                                    format!(
                                        "<b>Тема</b> {}\n<b>{}.</b> Вопрос скрыт",
                                        self.topic_title(),
                                        self.current_question().cost
                                    ),
                                )
                                .await;
                            self.send_message(format!("Ваш ответ, {}?", self.user_name(from)))
                                .await;
                            self.schedule_timeout(Self::ANSWER);
                        }
                    }
                    GameState::Answer(message_id, mut answers, current) => {
                        if *from == current {
                            if data == "+" {
                                return;
                            }
                            if self.current_question().check_answer(data.as_str()) {
                                answers.push(current);
                                self.game.game_state =
                                    GameState::AfterQuestion(false, answers, Some(current));
                                self.edit_message(&message_id).await;
                                self.send_message(format!(
                                    "Это правильный ответ, {}\n{}",
                                    self.user_name(&current),
                                    self.current_question().display_answers(true)
                                ))
                                .await;
                                self.schedule_timeout(Self::INTERMISSION);
                            } else {
                                self.incorrect_answer(false, false).await;
                            }
                        }
                    }
                    GameState::AfterQuestion(paused, answers, correct) => {
                        if tokens.is_empty() {
                            if (command == "yes" || command == "да")
                                && answers.contains(from)
                                && correct != Some(*from)
                            {
                                self.game.game_state =
                                    GameState::AfterQuestion(paused, answers, Some(*from));
                                self.send_message(format!("Принято, {}", self.user_name(from)))
                                    .await;
                                self.schedule_timeout(if paused {
                                    Self::PAUSE
                                } else {
                                    Self::INTERMISSION
                                });
                            } else if (command == "no" || command == "нет")
                                && correct == Some(*from)
                            {
                                self.game.game_state =
                                    GameState::AfterQuestion(paused, answers, None);
                                self.send_message(format!("Принято, {}", self.user_name(from)))
                                    .await;
                                self.schedule_timeout(if paused {
                                    Self::PAUSE
                                } else {
                                    Self::INTERMISSION
                                });
                            }
                        }
                    }
                    GameState::SpecialScore(_) => {}
                    GameState::AfterGame => {
                        self.schedule_timeout(Self::AFTER_GAME);
                    }
                }
            }
            MessageKind::NewChatMembers { data } => {
                if let GameState::BeforeGame(paused, _) = self.game.game_state {
                    for user in data {
                        let id = user.id.into();
                        match self.game.players.get_mut(&id) {
                            None => {
                                if !self.game.spectators.contains(&id) {
                                    self.play_bot
                                        .kick(ChatId::from(self.game.chat_id), user.id)
                                        .await;
                                }
                            }
                            Some(player_data) => player_data.2 = true,
                        }
                    }
                    if !self
                        .game
                        .players
                        .iter()
                        .any(|(_, (_, _, present))| !*present)
                    {
                        self.game.game_state = GameState::BeforeGame(paused, 1);
                        self.send_message("Игра скоро начнется".to_string()).await;
                        if !paused {
                            self.schedule_timeout(Self::PRE_GAME);
                        } else {
                            self.schedule_timeout(Self::PAUSE);
                        }
                    }
                } else {
                    for user in data {
                        let id = user.id.into();
                        if !self.game.players.contains_key(&id)
                            && !self.game.spectators.contains(&id)
                        {
                            self.play_bot
                                .kick(ChatId::from(self.game.chat_id), user.id)
                                .await;
                        }
                    }
                }
            }
            _ => {}
        };
        self.data.save_game_state(&self.game);
    }

    async fn show_score(&mut self) {
        let mut score_list = self
            .game
            .players
            .iter()
            .map(|(_, (data, score, _))| (data.display_name.clone(), *score))
            .collect::<Vec<_>>();
        score_list.sort_by(|(_, s1), (_, s2)| s2.cmp(s1));
        let mut result = String::new();
        for (name, score) in score_list {
            result += format!("{} {}\n", name, score).as_str();
        }
        self.send_message(format!(
            "<b>{} счёт:</b>\n{}",
            if self.game.current_topic == self.game.topics.len() {
                "Финальный"
            } else {
                "Teкущий"
            },
            result
        ))
        .await;
        self.schedule_timeout(Self::INTERMISSION);
    }

    fn topic_title(&self) -> String {
        encode(&self.current_topic().name)
    }

    async fn ask_question(&mut self) {
        self.game.game_state = GameState::BeforeQuestion(false);
        self.send_message("Внимание, вопрос".to_string()).await;
        self.schedule_timeout(Duration::from_secs(1));
    }

    fn current_question(&self) -> Question {
        self.current_topic().questions[self.game.current_question].fix()
    }

    fn current_topic(&self) -> &Topic {
        &self.topic_set.topics[self.game.topics[self.game.current_topic]]
    }

    fn is_last_topic(&self) -> bool {
        self.game.current_topic + 1 == self.game.topics.len()
    }

    fn question_text(&self) -> String {
        self.current_question()
            .display_question(&self.topic_title())
    }

    async fn advance_state(&mut self) -> bool {
        if self.game.game_state.set_pause(false) {
            self.send_message("Игра возобновлена".to_string()).await;
            self.schedule_timeout(Self::INTERMISSION);
        } else {
            match self.game.game_state.clone() {
                GameState::BeforeGame(_, minutes) => {
                    if minutes > 1
                        && !self
                            .play_bot
                            .all_players_in_chat(
                                ChatId::new(self.game.chat_id),
                                self.game
                                    .players
                                    .keys()
                                    .map(|id| UserId::new(*id))
                                    .collect::<Vec<_>>(),
                            )
                            .await
                    {
                        self.game.game_state = GameState::BeforeGame(false, minutes - 1);
                        self.send_message(format!(
                            "Некоторые игроки всё еще не зашли в чат. Через {} минут{} игра начнется автоматически",
                            minutes - 1,
                            if minutes == 2 { "у" } else { "ы" }
                        )).await;
                        self.schedule_timeout(Self::PRE_GAME_STEP);
                    } else {
                        self.game.game_state = GameState::BeforeTopic(false);
                        let mut list = "<b>Список тем:</b>\n".to_string();
                        for i in self.game.topics.iter() {
                            list +=
                                format!("{}. {}\n", i + 1, self.topic_set.topics[*i].name).as_str();
                        }
                        self.send_message(format!(
                            "Игра началась. Игроки:\n{}\n\n{}\n{}\n{}\n\n",
                            player_list(
                                &self
                                    .game
                                    .players
                                    .iter()
                                    .map(|(_, (player, ..))| player)
                                    .collect::<Vec<_>>()[..]
                            ),
                            self.topic_set.title,
                            self.topic_set.description,
                            list
                        ))
                        .await;
                        self.schedule_timeout(Self::INTERMISSION);
                    }
                }
                GameState::BeforeTopic(_) => {
                    if self.game.current_topic == self.game.topics.len() {
                        self.end_game(false).await;
                    } else {
                        self.game.game_state = GameState::BeforeFirstQuestion(false);
                        let remaining = self.game.topics.len() - self.game.current_topic;
                        self.send_message(format!(
                            "{}\n<b>Тема {}:</b> {}",
                            if remaining == 1 {
                                "Последняя тема".to_string()
                            } else {
                                format!("Осталось {}", Topic::topic_word(remaining))
                            },
                            self.game.topics[self.game.current_topic] + 1,
                            self.topic_title()
                        ))
                        .await;
                        self.schedule_timeout(Self::INTERMISSION);
                    }
                }
                GameState::BeforeFirstQuestion(_) => {
                    self.ask_question().await;
                }
                GameState::BeforeQuestion(_) => {
                    let id = self
                        .send_message_with_markup(self.question_text(), KeyboardOptions::None)
                        .await
                        .unwrap();
                    self.game.game_state = GameState::Question(id, Vec::new());
                    self.schedule_timeout(Self::FIRST_THINKING);
                }
                GameState::Question(_, _) => {
                    self.end_question(false).await;
                }
                GameState::Answer(_, _, _) => {
                    self.incorrect_answer(true, false).await;
                }
                GameState::AfterQuestion(_, answered, correct) => {
                    for id in answered.iter() {
                        if let Some(cid) = &correct {
                            if cid == id {
                                self.game.players.get_mut(id).unwrap().1 +=
                                    self.current_question().cost as i32;
                                break;
                            }
                        }
                        self.update_status();
                        self.game.players.get_mut(id).unwrap().1 -=
                            self.current_question().cost as i32;
                    }
                    self.game.current_question += 1;
                    if self.game.current_question == self.current_topic().questions.len() {
                        self.game.current_question = 0usize;
                        self.game.current_topic += 1;
                        self.update_status();
                        self.game.game_state = GameState::BeforeTopic(false);
                        self.show_score().await;
                    } else if self.is_last_topic()
                        && self.game.current_question + 2 >= self.current_topic().questions.len()
                    {
                        self.game.game_state = GameState::SpecialScore(false);
                        self.show_score().await;
                    } else {
                        self.ask_question().await;
                    }
                }
                GameState::SpecialScore(_) => {
                    self.ask_question().await;
                }
                GameState::AfterGame => {
                    self.data.remove_game(self.game.chat_id);
                    self.play_bot
                        .invalidate_invite_link(
                            ChatId::new(self.game.chat_id),
                            self.game.invite_link.clone(),
                        )
                        .await;
                    for user in self.game.players.keys().chain(self.game.spectators.iter()) {
                        self.play_bot
                            .kick(ChatId::from(self.game.chat_id), UserId::from(*user))
                            .await;
                    }
                    return true;
                }
            }
        }
        self.data.save_game_state(&self.game);
        false
    }

    pub async fn start_game(mut self, message_stream: UnboundedReceiverStream<Message>) {
        let mut event_stream = select_all(vec![
            message_stream.map(Event::Message).boxed(),
            self.timeout_stream.take().unwrap().boxed(),
        ]);
        self.process_starting_state().await;
        self.data.save_game_state(&self.game);
        while let Some(event) = event_stream.next().await {
            if match event {
                Event::Message(message) => {
                    self.process_message(message).await;
                    false
                }
                Event::Timeout(id) => {
                    if self.state_id == id {
                        self.advance_state().await
                    } else {
                        false
                    }
                }
            } {
                break;
            }
        }
        self.status_sender
            .send(StatusUpdate {
                chat_id: self.game.chat_id,
                update_type: UpdateType::GameEnded,
            })
            .unwrap();
    }

    async fn process_starting_state(&mut self) {
        match self.game.game_state.clone() {
            GameState::BeforeGame(_, minutes) => {
                assert!(minutes > 0);
                self.schedule_timeout(Self::PRE_GAME_STEP);
                self.game.game_state = GameState::BeforeGame(false, minutes);
            }
            GameState::BeforeTopic(_) => {
                self.schedule_timeout(Self::PAUSE);
                self.game.game_state = GameState::BeforeTopic(true);
            }
            GameState::BeforeFirstQuestion(_) => {
                self.schedule_timeout(Self::PAUSE);
                self.game.game_state = GameState::BeforeFirstQuestion(true);
            }
            GameState::BeforeQuestion(_) => {
                self.schedule_timeout(Self::PAUSE);
                self.game.game_state = GameState::BeforeQuestion(true);
            }
            GameState::Question(_, _) => {
                self.end_question(true).await;
            }
            GameState::Answer(_, _, _) => {
                self.incorrect_answer(false, true).await;
            }
            GameState::AfterQuestion(_, answers, correct_answer) => {
                self.schedule_timeout(Self::PAUSE);
                self.game.game_state =
                    GameState::AfterQuestion(true, answers.clone(), correct_answer.clone());
            }
            GameState::SpecialScore(_) => {
                self.schedule_timeout(Self::PAUSE);
                self.game.game_state = GameState::SpecialScore(true);
            }
            GameState::AfterGame => {
                self.schedule_timeout(Self::AFTER_GAME);
            }
        }
        if !self.game.game_state.before() {
            self.send_message(
                "Бот восстановлен после перезапуска. Он находится на паузе.".to_string(),
            )
            .await;
        }
    }

    fn cancel_timer(&mut self) {
        if let Some(handle) = self.timeout_handle.take() {
            handle.abort();
        }
    }
}
