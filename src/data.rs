use crate::game::Game;
use crate::topic::TopicSet;
use borsh::{BorshDeserialize, BorshSerialize};
use sled::transaction::{
    ConflictableTransactionResult, TransactionalTree, UnabortableTransactionError,
};
use sled::Db;
use std::collections::HashMap;
#[cfg(test)]
use std::fs::File;
#[cfg(test)]
use std::io::{BufRead, BufReader};
use std::ops::{Add, BitAnd, Shl, Shr};
use std::sync::{Arc, RwLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use telegram_bot::{ChatId, User, UserId};

pub enum UserBanResult {
    Banned,
    AlreadyInList,
    SizeLimitReached,
}

#[derive(Clone)]
pub struct Data {
    db: Db,
    sets: Arc<RwLock<HashMap<String, Arc<TopicSet>>>>,
}

impl Data {
    const NEXT_RESET_KEY: &'static str = "next-reset";
    const USER_DATA_KEY: &'static str = "user-data";
    const SETS_KEY: &'static str = "sets";
    const ACTIVE_SETS_KEY: &'static str = "active-sets";
    const WAS_ACTIVE_SETS_KEY: &'static str = "was-active-sets";
    const PLAYED_KEY: &'static str = "played";
    const COUNT_PLAYED_KEY: &'static str = "count-played";
    const GAME_STATE_KEY: &'static str = "game-state";
    const GAME_CHATS_KEY: &'static str = "game-chats";
    const BLOCKED_SET_KEY: &'static str = "blocked_set";
    const BAN_LIST_KEY: &'static str = "ban-list";
    const LAST_PLAYED_KEY: &'static str = "last-played";

    const SIZE_SUFFIX: &'static str = "size";

    const START_RATING: u32 = 15000;
    const MAX_BAN_LIST: usize = 50;
    const STORE_PLAYED: usize = 10;

    pub fn new(db: &str) -> Self {
        let res = Data {
            db: sled::open(db).unwrap(),
            sets: Arc::new(RwLock::new(HashMap::new())),
        };
        res.load_sets();
        res
    }

    pub fn wipe(&self) {
        self.db.clear().unwrap();
    }

    pub fn get_last_played(&self, user: UserId) -> Vec<UserId> {
        self.get_list::<i64>(&format!("{}#{}", Self::LAST_PLAYED_KEY, user))
            .iter()
            .map(|id| UserId::new(*id))
            .collect()
    }

    pub fn add_game(&self, users: &[UserId]) {
        for user in users {
            for other in users {
                if *user != *other {
                    self.add_played(user, other);
                }
            }
        }
    }

    fn add_played(&self, user: &UserId, other: &UserId) {
        let other = *other;
        let key = format!("{}#{}", Self::LAST_PLAYED_KEY, user);
        if self.remove_element::<i64>(&key, &other.into()) {
            self.add_element::<i64>(&key, &other.into());
        } else {
            if self.list_size(&key) == Self::STORE_PLAYED {
                self.remove_at::<i64>(&key, 0usize);
            }
            self.add_element::<i64>(&key, &other.into());
        }
    }

    pub fn in_ban_list(&self, user: UserId, other: UserId) -> bool {
        self.get_list::<i64>(&format!("{}#{}", Self::BAN_LIST_KEY, user))
            .contains(&other.into())
    }

    pub fn add_to_ban_list(&self, user: UserId, other: UserId) -> UserBanResult {
        if self.in_ban_list(user, other) {
            UserBanResult::AlreadyInList
        } else {
            let key = format!("{}#{}", Self::BAN_LIST_KEY, user);
            if self.list_size(&key) == Self::MAX_BAN_LIST {
                UserBanResult::SizeLimitReached
            } else {
                self.add_element::<i64>(&key, &other.into());
                UserBanResult::Banned
            }
        }
    }

    pub fn remove_from_ban_list(&self, user: UserId, other: UserId) -> bool {
        if self.in_ban_list(user, other) {
            self.remove_element::<i64>(&format!("{}#{}", Self::BAN_LIST_KEY, user), &other.into());
            true
        } else {
            false
        }
    }

    pub fn get_ban_list(&self, user: UserId) -> Vec<UserId> {
        self.get_list::<i64>(&format!("{}#{}", Self::BAN_LIST_KEY, user))
            .iter()
            .map(|id| UserId::new(*id))
            .collect()
    }

    pub fn is_set_blocked(&self, id: UserId, set: &String) -> bool {
        let res: Option<bool> = self.get(&format!("{}#{}#{}", Self::BLOCKED_SET_KEY, id, set));
        if let Some(_) = res {
            true
        } else {
            false
        }
    }

    //noinspection RsSelfConvention
    pub fn set_set_blocked(&self, id: UserId, set: &String, to_block: bool) -> bool {
        if self.is_set_blocked(id, &set) {
            if to_block {
                false
            } else {
                self.remove(&format!("{}#{}#{}", Self::BLOCKED_SET_KEY, id, set));
                true
            }
        } else {
            if to_block {
                self.insert(&format!("{}#{}#{}", Self::BLOCKED_SET_KEY, id, set), &true);
                true
            } else {
                false
            }
        }
    }

    pub fn update_player(&self, id: UserId, mut user_data: UserData) -> UserData {
        if let Some(old_data) = self.get_user_data(&id) {
            user_data.rating = old_data.rating;
        }
        self.set_user_data(id, &user_data);
        user_data
    }

    pub fn add_game_chat(&self, chat_id: &i64) {
        self.add_element(&Self::GAME_CHATS_KEY.to_string(), chat_id);
    }

    pub fn remove_game_chat(&self, chat_id: &i64) {
        self.remove_element(&Self::GAME_CHATS_KEY.to_string(), chat_id);
    }

    pub fn get_game_chats(&self) -> Vec<ChatId> {
        self.get_list::<i64>(&Self::GAME_CHATS_KEY.to_string())
            .iter()
            .map(|id| ChatId::new(*id))
            .collect()
    }

    pub fn save_game_state(&self, game: &Game) {
        self.insert(&format!("{}#{}", Self::GAME_STATE_KEY, game.chat_id), game);
    }

    pub fn get_game_states(&self) -> Vec<Game> {
        self.db
            .scan_prefix(Self::GAME_STATE_KEY)
            .map(|r| match r {
                Ok((_, value)) => Game::deserialize(&mut value.as_ref()).unwrap(),
                Err(err) => panic!("Error while working with db {}", err),
            })
            .collect()
    }

    pub fn remove_game(&self, id: i64) {
        self.remove(&format!("{}#{}", Self::GAME_STATE_KEY, id));
    }

    pub fn save_game_results(&self, results: &HashMap<i64, (UserData, i32, bool)>) {
        self.transaction(|db| {
            let results = results.iter().collect::<Vec<_>>();
            let mut datas = Vec::new();
            for user in results.iter() {
                let mut data: UserData =
                    Self::get_tree(db, &format!("{}#{}", Self::USER_DATA_KEY, user.0)).unwrap();
                let ra = data.rating;
                let mut delta = 0i32;
                for other in results.iter() {
                    if user.0 == other.0 {
                        continue;
                    }
                    let other_data: UserData =
                        Self::get_tree(db, &format!("{}#{}", Self::USER_DATA_KEY, other.0))
                            .unwrap();
                    let rb = other_data.rating;
                    let ea = 1f64 / (1f64 + 10f64.powf(((rb as f64) - (ra as f64)) / 4000f64));
                    let sa = if user.1 .1 < other.1 .1 {
                        0f64
                    } else if user.1 .1 > other.1 .1 {
                        1f64
                    } else {
                        0.5f64
                    };
                    delta += (100f64 * (sa - ea)).round() as i32;
                }
                delta = delta.max(-(ra as i32) + 10);
                data.rating = (data.rating as i32 + delta) as u32;
                datas.push((*user.0, data));
            }
            for (user_id, data) in datas {
                Self::insert_tree(db, &format!("{}#{}", Self::USER_DATA_KEY, &user_id), &data)?;
            }
            Ok(())
        })
    }

    pub fn get_next_reset(&self) -> SystemTime {
        self.get_raw(Self::NEXT_RESET_KEY.as_bytes())
            .map(|time| UNIX_EPOCH.add(Duration::from_millis(time)))
            .unwrap_or_else(|| SystemTime::now())
    }

    //noinspection RsSelfConvention
    pub fn set_next_reset(&self, time: SystemTime) {
        self.insert_raw(
            Self::NEXT_RESET_KEY.as_bytes(),
            &(time.duration_since(UNIX_EPOCH).unwrap().as_millis() as u64),
        );
    }

    pub fn get_user_data(&self, id: &UserId) -> Option<UserData> {
        self.get(&format!("{}#{}", Self::USER_DATA_KEY, id))
    }

    pub fn get_or_create_user(&self, user: User) -> UserData {
        let mut user_data = match self.get_user_data(&user.id) {
            None => UserData {
                display_name: "".to_string(),
                rating: Self::START_RATING,
            },
            Some(user_data) => user_data,
        };
        user_data.display_name = display_name(&user);
        user_data
    }

    //noinspection RsSelfConvention
    pub fn set_user_data(&self, id: UserId, user_data: &UserData) {
        self.insert(&format!("{}#{}", Self::USER_DATA_KEY, id), user_data);
    }

    pub fn rating_discount(&self) {
        let users = self.db.scan_prefix(Self::USER_DATA_KEY).collect::<Vec<_>>();
        self.transaction(|db| {
            for item in users.iter() {
                match item {
                    Ok((key, value)) => {
                        let mut user_data = UserData::deserialize(&mut value.as_ref()).unwrap();
                        user_data.rating =
                            Self::START_RATING + (user_data.rating - Self::START_RATING) * 99 / 100;
                        Self::insert_tree_raw(db, key.as_ref(), &user_data)?;
                    }
                    Err(err) => panic!("Error while working with db {}", err),
                }
            }
            Ok(())
        })
    }

    pub fn add_new_set(&self, id: &String, set: TopicSet) -> bool {
        if self.was_active(&id) && set.topics.len() != self.get_set(id).unwrap().topics.len() {
            return false;
        }
        self.insert(&format!("{}#{}", Self::SETS_KEY, id), &set);
        self.sets.write().unwrap().insert(id.clone(), Arc::new(set));
        true
    }

    pub fn get_active_set_ids(&self) -> Vec<String> {
        self.get_list(&Self::ACTIVE_SETS_KEY.to_string())
    }

    pub fn get_was_active_set_ids(&self) -> Vec<String> {
        self.get_list(&Self::WAS_ACTIVE_SETS_KEY.to_string())
    }

    pub fn is_active(&self, set_id: &String) -> bool {
        self.get_active_set_ids().contains(set_id)
    }

    pub fn was_active(&self, set_id: &String) -> bool {
        self.get_was_active_set_ids().contains(set_id)
    }

    pub fn add_active(&self, set_id: &String) {
        self.add_element(&Self::WAS_ACTIVE_SETS_KEY.to_string(), set_id);
        self.add_element(&Self::ACTIVE_SETS_KEY.to_string(), set_id);
    }

    pub fn remove_active(&self, set_id: &String) {
        self.remove_element(&Self::ACTIVE_SETS_KEY.to_string(), set_id);
    }

    pub fn get_rating_list(&self, top: usize) -> String {
        let mut users: Vec<UserData> = self
            .db
            .scan_prefix(Self::USER_DATA_KEY)
            .map(|result| match result {
                Ok((_, value)) => UserData::deserialize(&mut value.as_ref()).unwrap(),
                Err(err) => panic!("Error while working with db {}", err),
            })
            .collect::<Vec<_>>();
        users.sort_by(|u1, u2| u2.rating.cmp(&u1.rating));
        let mut res = String::new();
        let mut place = 1usize;
        let mut last_rating = 0u32;
        let mut add = 0usize;
        for user_data in users.iter() {
            if user_data.rating != last_rating {
                last_rating = user_data.rating;
                place += add;
                if place > top {
                    break;
                }
                add = 0;
            }
            add += 1;
            res += format!(
                "<b>{}.</b> {} {}\n",
                place,
                user_data.display_name(),
                display_rating(user_data.rating)
            )
            .as_str();
        }
        res
    }

    pub fn get_set(&self, id: &String) -> Option<Arc<TopicSet>> {
        self.sets.read().unwrap().get(id).map(|set| set.clone())
    }

    pub fn get_played(&self, user_id: UserId, set_id: &String) -> Option<BitSet> {
        self.get(&format!("{}#{}#{}", Self::PLAYED_KEY, user_id, set_id))
    }

    fn get_count_played(&self, user_id: UserId, set_id: &String) -> usize {
        self.get(&format!("{}#{}#{}", Self::PLAYED_KEY, user_id, set_id))
            .unwrap_or(0usize)
    }

    pub fn topics_in_set_remain(&self, user_id: UserId, set_id: &String) -> usize {
        if self.is_set_blocked(user_id, set_id) {
            0
        } else {
            self.get_set(set_id).unwrap().topics.len() - self.get_count_played(user_id, set_id)
        }
    }

    //noinspection RsSelfConvention
    pub fn set_played(&self, users: &[&UserId], set_id: &String, topics: &[usize]) {
        self.transaction(|db| {
            let topic_count = self.get_set(set_id).unwrap().topics.len();
            for user_id in users.iter() {
                let mut played =
                    Self::get_tree(db, &format!("{}#{}#{}", Self::PLAYED_KEY, user_id, set_id))
                        .unwrap_or_else(|| BitSet::new(topic_count));
                for id in topics.iter() {
                    played.set_bit(*id);
                }
                Self::insert_tree(
                    db,
                    &format!("{}#{}#{}", Self::PLAYED_KEY, user_id, set_id),
                    &played,
                )?;
                Self::insert_tree(
                    db,
                    &format!("{}#{}#{}", Self::COUNT_PLAYED_KEY, user_id, set_id),
                    &played.size,
                )?;
            }
            Ok(())
        });
    }

    fn load_sets(&self) {
        let mut guard = self.sets.write().unwrap();
        for item in self.db.scan_prefix(Self::SETS_KEY) {
            match item {
                Ok((_, value)) => {
                    let set = TopicSet::deserialize(&mut value.as_ref()).unwrap();
                    guard.insert(set.id.clone(), Arc::new(set));
                }
                Err(err) => panic!("Error while working with db {}", err),
            }
        }
    }

    fn get<T: BorshSerialize + BorshDeserialize>(&self, key: &String) -> Option<T> {
        self.get_raw(key.as_bytes())
    }

    //noinspection RsSelfConvention
    fn get_tree<T: BorshSerialize + BorshDeserialize>(
        db: &TransactionalTree,
        key: &String,
    ) -> Option<T> {
        Self::get_tree_raw(db, key.as_bytes())
    }

    fn get_raw<T: BorshSerialize + BorshDeserialize>(&self, key: &[u8]) -> Option<T> {
        match self.db.get(key) {
            Ok(result) => match result {
                None => None,
                Some(result) => Some(T::deserialize(&mut result.as_ref()).unwrap()),
            },
            Err(err) => panic!("Error while working with db {}", err),
        }
    }

    //noinspection RsSelfConvention
    fn get_tree_raw<T: BorshSerialize + BorshDeserialize>(
        db: &TransactionalTree,
        key: &[u8],
    ) -> Option<T> {
        match db.get(key) {
            Ok(result) => match result {
                None => None,
                Some(result) => Some(T::deserialize(&mut result.as_ref()).unwrap()),
            },
            Err(err) => panic!("Error while working with db {}", err),
        }
    }

    fn insert<T: BorshSerialize + BorshDeserialize>(&self, key: &String, element: &T) {
        self.insert_raw(key.as_bytes(), element)
    }

    fn insert_raw<T: BorshSerialize + BorshDeserialize>(&self, key: &[u8], element: &T) {
        match self.db.insert(key, element.try_to_vec().unwrap()) {
            Ok(_) => {}
            Err(err) => panic!("Error while working with db {}", err),
        }
    }

    fn insert_tree<T: BorshSerialize + BorshDeserialize>(
        db: &TransactionalTree,
        key: &String,
        element: &T,
    ) -> Result<(), UnabortableTransactionError> {
        Self::insert_tree_raw(db, key.as_bytes(), element)
    }

    fn insert_tree_raw<T: BorshSerialize + BorshDeserialize>(
        db: &TransactionalTree,
        key: &[u8],
        element: &T,
    ) -> Result<(), UnabortableTransactionError> {
        db.insert(key, element.try_to_vec().unwrap())?;
        Ok(())
    }

    fn remove(&self, key: &String) {
        self.remove_raw(key.as_bytes())
    }

    fn remove_raw(&self, key: &[u8]) {
        match self.db.remove(key) {
            Ok(_) => {}
            Err(err) => panic!("Error while working with db {}", err),
        }
    }

    fn remove_tree(
        db: &TransactionalTree,
        key: &String,
    ) -> Result<(), UnabortableTransactionError> {
        Self::remove_tree_raw(db, key.as_bytes())
    }

    fn remove_tree_raw(
        db: &TransactionalTree,
        key: &[u8],
    ) -> Result<(), UnabortableTransactionError> {
        db.remove(key)?;
        Ok(())
    }

    fn transaction<F>(&self, f: F)
    where
        F: Fn(&TransactionalTree) -> ConflictableTransactionResult<(), UnabortableTransactionError>,
    {
        match self.db.transaction(f) {
            Ok(_) => {}
            Err(err) => panic!("Error while working with db {}", err),
        }
    }

    fn get_list<T: BorshSerialize + BorshDeserialize>(&self, key: &String) -> Vec<T> {
        let len = self
            .get(&&format!("{}#{}", key, Self::SIZE_SUFFIX))
            .unwrap_or(0usize);
        let mut res = Vec::new();
        for i in 0usize..len {
            res.push(self.get(&format!("{}#{}", key, i)).unwrap());
        }
        res
    }

    fn add_element<T: BorshSerialize + BorshDeserialize>(&self, key: &String, element: &T) {
        let len = self
            .get::<usize>(&format!("{}#{}", key, Self::SIZE_SUFFIX))
            .unwrap_or(0usize);
        self.transaction(|db| {
            Self::insert_tree(db, &format!("{}#{}", key, len), element)?;
            Self::insert_tree(db, &format!("{}#{}", key, Self::SIZE_SUFFIX), &(len + 1))?;
            Ok(())
        });
    }

    fn remove_element<T: BorshSerialize + BorshDeserialize + PartialEq>(
        &self,
        key: &String,
        element: &T,
    ) -> bool {
        let len = self.list_size(key);
        for i in 0usize..len {
            if self.get::<T>(&format!("{}#{}", key, i)).unwrap() == *element {
                self.remove_at::<T>(key, i);
                return true;
            }
        }
        false
    }

    fn remove_at<T: BorshSerialize + BorshDeserialize>(&self, key: &String, i: usize) {
        let len = self.list_size(key) - 1;
        self.transaction(|db| {
            for j in i..len {
                Self::insert_tree(
                    db,
                    &format!("{}#{}", key, j),
                    &Self::get_tree::<T>(db, &format!("{}#{}", key, j + 1)).unwrap(),
                )?;
            }
            Self::remove_tree(db, &format!("{}#{}", key, len))?;
            Self::insert_tree(db, &format!("{}#{}", key, Self::SIZE_SUFFIX), &len)?;
            Ok(())
        });
    }

    fn list_size(&self, key: &String) -> usize {
        self.get(&format!("{}#{}", key, Self::SIZE_SUFFIX))
            .unwrap_or(0usize)
    }
}

#[derive(BorshSerialize, BorshDeserialize, Clone, Debug)]
pub struct UserData {
    display_name: String,
    pub rating: u32,
}

impl UserData {
    pub fn display_name(&self) -> String {
        html_escape::encode_text(&self.display_name).to_string()
    }
}

#[derive(BorshSerialize, BorshDeserialize)]
pub struct BitSet {
    pub size: usize,
    set: Vec<u8>,
}

impl BitSet {
    pub fn new(len: usize) -> Self {
        Self {
            size: 0usize,
            set: vec![0u8; (len + 7) / 8],
        }
    }

    pub fn set_bit(&mut self, index: usize) {
        if self.is_set(index) {
            return;
        }
        self.size += 1;
        self.set[index / 8] += 1u8.shl(index % 8);
    }

    pub fn is_set(&self, index: usize) -> bool {
        self.set[index / 8].shr(index % 8).bitand(1) == 1
    }

    pub fn unite(&mut self, right: &BitSet) {
        assert_eq!(self.set.len(), right.set.len());
        self.size = 0;
        for i in 0..self.set.len() {
            self.set[i] |= right.set[i];
            self.size += self.set[i].count_ones() as usize;
        }
    }
}

pub fn display_rating(rating: u32) -> u32 {
    (rating + 5) / 10
}

pub fn display_name(user: &User) -> String {
    match &user.last_name {
        None => user.first_name.clone(),
        Some(last_name) => format!("{} {}", user.first_name, last_name),
    }
}

#[test]
fn test_rating() {
    let mut results = Vec::new();
    for i in 0..3 {
        results.push((
            i as i64,
            (
                UserData {
                    display_name: i.to_string(),
                    rating: 15000,
                },
                i * 10 as i32,
                true,
            ),
        ));
    }
    for user in results.iter() {
        let ra = user.1 .0.rating;
        let mut delta = 0i32;
        for other in results.iter() {
            if user.0 == other.0 {
                continue;
            }
            let rb = other.1 .0.rating;
            let ea = 1f64 / (1f64 + 10f64.powf(((rb as f64) - (ra as f64)) / 400f64));
            let sa = if user.1 .1 < other.1 .1 {
                0f64
            } else if user.1 .1 > other.1 .1 {
                1f64
            } else {
                0.5f64
            };
            delta += (100f64 * (sa - ea)).round() as i32;
        }
        delta = delta.max(-(ra as i32) + 10);
        println!(
            "{} {} {}",
            user.0,
            delta,
            display_rating(((user.1 .0.rating as i32) + delta) as u32)
        );
    }
}

#[test]
fn main() {
    env_logger::Builder::new()
        .filter(None, log::LevelFilter::Info)
        .format_timestamp_millis()
        .write_style(env_logger::WriteStyle::Always)
        .init();

    log::info!("{}", std::env::current_dir().unwrap().to_str().unwrap());
    let data = Data::new("migrated.db");
    data.wipe();

    let sets = read_file("active.list");
    let mut active = std::collections::HashSet::new();
    for id in sets {
        if id.starts_with("#") {
            continue;
        }
        active.insert(id.clone());
        let set = crate::parser::parse_json(
            id.clone(),
            std::fs::read_to_string(format!("../olddb/{}.json", &id)).unwrap(),
        )
        .unwrap();
        data.add_new_set(&id, set);
        data.add_active(&id);
        log::info!("set {} loaded", id);
    }
    // let active = read_file("active.list");
    // for id in active.iter() {
    //     data.add_active(id);
    // }
    // let active = active.into_iter().collect::<HashSet<_>>();
    let players = read_file("player.list");
    let mut users = HashMap::new();
    for i in (0..players.len()).step_by(3) {
        let id = players[i].parse::<i64>().unwrap();
        let display_name = players[i + 1].clone();
        let rating = 10 * players[i + 2].parse::<u32>().unwrap();
        users.insert(
            id,
            UserData {
                display_name,
                rating,
            },
        );
    }
    let player_count = players.len() / 3;
    let played = read_file("played.list");
    let mut it = played.iter();
    let mut at = 0usize;
    while let Some(next) = it.next() {
        let id = next.parse::<i64>().unwrap();
        let count = it.next().unwrap().parse::<usize>().unwrap();
        let mut played = HashMap::new();
        for _ in 0usize..count {
            let set_id = it.next().unwrap().clone();
            let topic = it.next().unwrap().parse::<usize>().unwrap();
            if active.contains(&set_id) && data.get_set(&set_id).unwrap().topics.len() > topic {
                if !played.contains_key(&set_id) {
                    played.insert(set_id.clone(), Vec::new());
                }
                played.get_mut(&set_id).unwrap().push(topic);
            }
        }
        if !played.is_empty() {
            if let Some(user) = users.get(&id) {
                data.set_user_data(UserId::new(id), user);
            }
        }
        for (set_id, topics) in played {
            data.set_played(&vec![&UserId::new(id)], &set_id, &topics);
        }
        at += 1;
        log::info!("User processed {}/{}", at, player_count);
    }
}

#[cfg(test)]
fn read_file(filename: &str) -> Vec<String> {
    let path = format!("../olddb/{}", filename);
    log::info!("{}", path);
    BufReader::new(File::open(path).unwrap())
        .lines()
        .map(|res| res.unwrap())
        .collect()
}
