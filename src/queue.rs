use crate::bot::TelegramBot;
use crate::data::{Data, UserData};
use crate::{find_topics, GameStartData};
use futures::stream::select_all;
use futures::StreamExt;
use std::collections::HashMap;
use std::time::{Duration, Instant};
use telegram_bot::{MessageId, UserId};
use tokio::sync::mpsc::{unbounded_channel, UnboundedSender};
use tokio::task::JoinHandle;
use tokio_stream::wrappers::UnboundedReceiverStream;

#[derive(Debug)]
pub enum UpdateMessage {
    UserEntered(UserId),
    UserLeft(UserId),
    Shutdown,
    FindGames,
}

#[derive(Debug)]
struct QueueEntry {
    user_id: UserId,
    user_data: UserData,
    min_rating: i64,
    max_rating: i64,
    will_play_with_three: bool,
}

struct GameFinder<'s> {
    players: &'s Vec<QueueEntry>,
    num_players: usize,
    result: Vec<&'s QueueEntry>,
    data: Data,
}

impl<'s> GameFinder<'s> {
    pub fn find_game(
        players: &'s Vec<QueueEntry>,
        num_players: usize,
        data: Data,
    ) -> Option<(GameStartData, String, Vec<usize>)> {
        let mut game_finder = Self {
            players,
            num_players,
            result: Vec::new(),
            data,
        };
        game_finder.do_find_game(num_players, players.len())
    }

    fn compatible(&self, one: &'s QueueEntry, another: &'s QueueEntry) -> bool {
        one.max_rating >= another.user_data.rating as i64
            && one.min_rating <= another.user_data.rating as i64
            && another.max_rating >= one.user_data.rating as i64
            && another.min_rating <= another.user_data.rating as i64
            && !self.data.in_ban_list(one.user_id, another.user_id)
            && !self.data.in_ban_list(another.user_id, one.user_id)
    }

    fn do_find_game(
        &mut self,
        left_players: usize,
        limit: usize,
    ) -> Option<(GameStartData, String, Vec<usize>)> {
        if left_players == 0 {
            let mut game_start_data = GameStartData {
                chat_ids: self
                    .result
                    .iter()
                    .map(|entry| entry.user_id.into())
                    .collect(),
                set_id: None,
                topic_count: 6,
                players: self
                    .result
                    .iter()
                    .map(|entry| (entry.user_id, entry.user_data.clone()))
                    .collect(),
                spectators: HashMap::new(),
            };
            find_topics(&self.data, &mut game_start_data)
                .map(|(set_id, topics)| (game_start_data, set_id, topics))
        } else {
            if left_players > limit {
                None
            } else {
                for next in (left_players - 1)..limit {
                    if self.num_players == 4 || self.players[next].will_play_with_three {
                        let mut good = true;
                        for player in self.result.iter() {
                            if !self.compatible(player, &self.players[next]) {
                                good = false;
                                break;
                            }
                        }
                        if good {
                            self.result.push(&self.players[next]);
                            if let Some(res) = self.do_find_game(left_players - 1, next) {
                                return Some(res);
                            }
                            self.result.pop();
                        }
                    }
                }
                None
            }
        }
    }
}

pub struct PlayQueue {
    data: Data,
    bot: TelegramBot,
    queue: Vec<(UserId, MessageId, Instant, Instant)>,
    handle: Option<JoinHandle<()>>,
    sender: UnboundedSender<(GameStartData, String, Vec<usize>)>,
    update_stream: Option<UnboundedReceiverStream<UpdateMessage>>,
    find_sender: UnboundedSender<UpdateMessage>,
    find_stream: Option<UnboundedReceiverStream<UpdateMessage>>,
}

impl PlayQueue {
    pub fn new(
        data: Data,
        scheduler_bot: TelegramBot,
        update_stream: UnboundedReceiverStream<UpdateMessage>,
    ) -> (
        Self,
        UnboundedReceiverStream<(GameStartData, String, Vec<usize>)>,
    ) {
        let (game_sender, game_receiver) = unbounded_channel();
        let (find_sender, find_receiver) = unbounded_channel();
        (
            Self {
                data,
                bot: scheduler_bot,
                queue: Vec::new(),
                handle: None,
                sender: game_sender,
                update_stream: Some(update_stream),
                find_sender,
                find_stream: Some(UnboundedReceiverStream::new(find_receiver)),
            },
            UnboundedReceiverStream::new(game_receiver),
        )
    }

    fn find_in_queue(&self, user_id: UserId) -> Option<usize> {
        for (i, (id, ..)) in self.queue.iter().enumerate() {
            if *id == user_id {
                return Some(i);
            }
        }
        None
    }

    fn find_games(&mut self) {
        if self.queue.len() < 3 {
            return;
        }
        let players = self
            .queue
            .iter()
            .map(|(user_id, _, entered, _)| {
                let since_entered = entered.elapsed();
                let user_data = self.data.get_user_data(user_id).unwrap();
                let delta = (since_entered.as_millis() / 100) as i64;
                let min_rating = (user_data.rating as i64) - delta;
                let max_rating = (user_data.rating as i64) + delta;
                QueueEntry {
                    user_id: *user_id,
                    user_data,
                    min_rating,
                    max_rating,
                    will_play_with_three: since_entered >= Duration::from_secs(60),
                }
            })
            .collect();
        let mut min_num_players = 3usize;
        for (_, _, added, _) in self.queue.iter() {
            if added.elapsed() < Duration::from_secs(60) {
                min_num_players = 4usize;
                break;
            }
        }
        for num_players in (min_num_players..=4usize).rev() {
            if let Some(res) = GameFinder::find_game(&players, num_players, self.data.clone()) {
                for user_id in res.0.players.keys() {
                    self.queue.remove(self.find_in_queue(*user_id).unwrap());
                }
                self.update_messages();
                self.sender.send(res).unwrap();
                return;
            }
        }
    }

    fn update_messages(&self) {
        for (user_id, message_id, ..) in self.queue.iter() {
            self.bot.try_edit_message(
                (*user_id).into(),
                *message_id,
                Self::queue_message_text(self.queue.len()),
            );
        }
    }

    pub async fn start(&mut self) {
        let mut stream = select_all(vec![
            self.update_stream.take().unwrap().boxed(),
            self.find_stream.take().unwrap().boxed(),
        ]);
        self.schedule_timeout();
        while let Some(message) = stream.next().await {
            match message {
                UpdateMessage::UserEntered(user_id) => self.user_entered(user_id).await,
                UpdateMessage::UserLeft(user_id) => match self.find_in_queue(user_id) {
                    None => {
                        self.bot.try_send_message(
                            user_id.into(),
                            "Вы не находились в очереди".to_string(),
                        );
                    }
                    Some(at) => {
                        self.queue.remove(at);
                        self.bot
                            .try_send_message(user_id.into(), "Вы вышли из очереди".to_string());
                        self.update_messages();
                    }
                },
                UpdateMessage::Shutdown => {
                    self.cancel_timer();
                    break;
                }
                UpdateMessage::FindGames => {
                    self.find_games();
                    let mut removed = false;
                    self.queue = self
                        .queue
                        .drain(..)
                        .filter(|(id, _, _, last_update)| {
                            let retain = last_update.elapsed() <= Duration::from_secs(600);
                            if !retain {
                                self.bot.try_send_message(
                                    (*id).into(),
                                    "Игра не найдена за 10 минут".to_string(),
                                );
                                removed = true;
                            }
                            retain
                        })
                        .collect();
                    if removed {
                        self.update_messages();
                    }
                    self.schedule_timeout();
                }
            }
        }
    }

    async fn user_entered(&mut self, user_id: UserId) {
        match self.find_in_queue(user_id) {
            Some(at) => {
                self.queue[at].3 = Instant::now();
                if let Some(message_id) = self
                    .bot
                    .try_send_once(user_id.into(), Self::queue_message_text(self.queue.len()))
                    .await
                {
                    self.queue[at].1 = message_id;
                }
            }
            None => {
                if let Some(message_id) = self
                    .bot
                    .try_send_once(user_id.into(), "Вы добавлены в очередь".to_string())
                    .await
                {
                    self.queue
                        .push((user_id, message_id, Instant::now(), Instant::now()));
                    self.update_messages();
                }
            }
        }
    }

    fn queue_message_text(in_queue: usize) -> String {
        format!("Ищем игру. Всего игроков в очереди <b>{}</b>", in_queue)
    }

    fn cancel_timer(&mut self) {
        if let Some(handle) = self.handle.take() {
            handle.abort();
        }
    }

    fn schedule_timeout(&mut self) {
        self.cancel_timer();
        let sender = self.find_sender.clone();
        self.handle = Some(tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(1)).await;
            match sender.send(UpdateMessage::FindGames) {
                Ok(_) => {}
                Err(err) => log::error!("Error with sending update: {}", err),
            }
        }))
    }
}
