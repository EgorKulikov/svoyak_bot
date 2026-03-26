use borsh::{BorshDeserialize, BorshSerialize};
use html_escape::encode_text;
use serde::{Deserialize, Serialize};

#[derive(BorshSerialize, BorshDeserialize, Clone, Deserialize, Serialize)]
pub struct Question {
    pub cost: u16,
    pub question: String,
    pub answers: Vec<String>,
    pub comment: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub form: Option<String>,
}

impl Question {
    pub fn new(cost: u16, question: String, answers: &[String], comment: Option<String>) -> Self {
        Self {
            cost,
            question: encode(&question),
            answers: answers.iter().map(|ans| encode(ans)).collect(),
            comment: comment.map(|comment| encode(&comment)),
            form: None,
        }
    }

    pub fn check_answer(&self, answer: &str) -> bool {
        let answer = answer.trim();
        let answer_no_par = Self::no_space(answer, false);
        let answer_par = Self::no_space(answer, true);
        self.answers.iter().any(|expected| {
            let expected_no_par = Self::no_space(expected.as_str(), false);
            let expected_par = Self::no_space(expected.as_str(), true);
            answer_no_par == expected_no_par
                || answer_no_par == expected_par
                || answer_par == expected_no_par
                || answer_par == expected_par
        })
    }

    pub fn display_question(&self, topic_name: &str) -> String {
        format!(
            "<b>Тема</b> {}\n<b>{}.</b> {}",
            encode_text(topic_name),
            self.cost,
            encode_text(&self.question)
        )
    }

    pub fn display_question_partial(&self, topic_name: &str, words_shown: usize) -> String {
        let words: Vec<&str> = self.question.split_whitespace().collect();
        let shown = words[..words_shown.min(words.len())].join(" ");
        format!(
            "<b>Тема</b> {}\n<b>{}.</b> {}",
            encode_text(topic_name),
            self.cost,
            encode_text(&shown)
        )
    }

    pub fn word_count(&self) -> usize {
        self.question.split_whitespace().count()
    }

    pub fn word_char_len(&self, index: usize) -> usize {
        self.question.split_whitespace().nth(index).map_or(0, |w| w.chars().count())
    }

    pub fn fix(&self) -> Question {
        Question {
            cost: self.cost,
            question: self.question.clone(),
            answers: self.answers.clone(),
            comment: self.comment.clone(),
            form: self.form.clone(),
        }
    }

    pub fn display_answers(&self, after_right_answer: bool) -> String {
        let mut res = if after_right_answer {
            "<b>Авторский ответ</b>: "
        } else {
            "<b>Ответ:</b> "
        }
        .to_string();
        let mut first = true;
        for answer in self.answers.iter() {
            if first {
                first = false;
            } else {
                res += "\n<b>Зачет</b>: ";
            }
            res += encode_text(answer).as_ref();
        }
        if let Some(comment) = &self.comment {
            res += "\n<b>Комментарий</b>: ";
            res += encode_text(comment).as_ref();
        }
        res
    }

    fn no_space(answer: &str, skip_parenthesis: bool) -> String {
        let mut res = String::new();
        let mut level = 0i16;
        for c in answer.chars() {
            if c == '(' || c == '[' || c == '{' {
                level += 1;
            } else if c == ')' || c == ']' || c == '}' {
                level -= 1;
            } else if (level == 0 || !skip_parenthesis) && c.is_alphanumeric() {
                if c == 'ё' || c == 'Ё' {
                    res.push('е');
                } else {
                    for d in c.to_lowercase() {
                        res.push(d);
                    }
                }
            }
        }
        res
    }
}

#[derive(BorshSerialize, BorshDeserialize, Clone, Deserialize, Serialize)]
pub struct Topic {
    pub name: String,
    pub questions: Vec<Question>,
}

impl Topic {
    pub fn new(name: String, questions: Vec<Question>) -> Self {
        Topic {
            name: encode(&name),
            questions,
        }
    }

    pub fn topic_word(topics: usize) -> String {
        if topics % 10 == 0 || topics % 10 >= 5 || topics % 100 >= 10 && topics % 100 < 20 {
            format!("{} тем", topics)
        } else if topics % 10 == 1 {
            format!("{} тема", topics)
        } else {
            format!("{} темы", topics)
        }
    }
}

#[derive(BorshSerialize, BorshDeserialize, Deserialize, Serialize)]
pub struct TopicSet {
    pub id: String,
    pub title: String,
    pub description: String,
    pub topics: Vec<Topic>,
}

impl TopicSet {
    pub fn new(id: String, title: String, description: String, topics: Vec<Topic>) -> Self {
        Self {
            id,
            title: encode(&title),
            description: encode(&description),
            topics,
        }
    }
}

pub fn encode(s: &String) -> String {
    encode_text(s.as_str()).to_string()
}
