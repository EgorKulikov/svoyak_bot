use crate::topic::{Question, Topic, TopicSet};
use core::option::Option::{None, Some};

struct LineCollection<'s> {
    lines: Vec<&'s str>,
    pos: usize,
}

impl<'s> LineCollection<'s> {
    pub fn new(lines: Vec<&'s str>) -> Self {
        LineCollection { lines, pos: 0usize }
    }

    pub fn next(&mut self) -> Option<Vec<&'s str>> {
        if self.pos == self.lines.len() {
            None
        } else {
            let mut res = Vec::new();
            while self.pos < self.lines.len() && !self.lines[self.pos].is_empty() {
                res.push(self.lines[self.pos]);
                self.pos += 1;
            }
            if self.pos != self.lines.len() {
                self.pos += 1;
            }
            Some(res)
        }
    }
}

fn get_part(lines: &mut Vec<&str>, from: &str, to: &str) -> Option<String> {
    let mut res = String::new();
    let mut on = false;
    for (i, s) in lines.iter().enumerate() {
        if on {
            if s.starts_with(to) {
                *lines = lines[i..].to_vec();
                break;
            }
            res += "\n";
            res += s;
        } else {
            if s.starts_with(from) {
                on = true;
                res += &s[from.len()..];
            }
        }
    }
    if on {
        Some(res)
    } else {
        None
    }
}

pub fn parse_pretty(id: String, content: String) -> Option<TopicSet> {
    let mut lc = LineCollection::new(content.split("\r\n").collect());
    if lc.next().is_none() {
        return None;
    }
    let title = match lc.next() {
        Some(title) => title.join("\n"),
        None => {
            return None;
        }
    };
    let description = match lc.next() {
        Some(description) => description.join("\n"),
        None => {
            return None;
        }
    };
    let mut topics = Vec::new();
    while let Some(mut topic) = lc.next() {
        let title = match get_part(&mut topic, "Тема ", "10. ") {
            None => {
                continue;
            }
            Some(title) => title,
        };
        let mut questions = Vec::new();
        let mut good = true;
        for i in 1..=5 {
            let question = match get_part(&mut topic, format!("{}. ", i * 10).as_str(), "Ответ: ")
            {
                None => {
                    good = false;
                    break;
                }
                Some(question) => question,
            };
            let answer = match get_part(
                &mut topic,
                "Ответ: ",
                format!("{}. ", (i + 1) * 10).as_str(),
            ) {
                None => {
                    good = false;
                    break;
                }
                Some(question) => question,
            };
            questions.push(Question::new(i * 10 as u16, question, &vec![answer], None));
        }
        if good {
            topics.push(Topic::new(title, questions));
        }
    }
    if topics.is_empty() {
        None
    } else {
        Some(TopicSet::new(id, title, description, topics))
    }
}

pub fn parse_json(id: String, content: String) -> Option<TopicSet> {
    match serde_json::from_str::<TopicSet>(content.as_str()) {
        Ok(set) => Some(set),
        Err(err) => {
            log::error!("Error parsing {} {}", id, err);
            None
        }
    }
}

pub fn parse(mut id: String, content: String) -> Option<TopicSet> {
    if id.ends_with(".json") {
        parse_json(id[..id.len() - 5].to_string(), content)
    } else {
        match id.find(".") {
            None => {}
            Some(pos) => {
                id = id[0..pos].to_string();
            }
        }
        parse_pretty(id, content)
    }
}
