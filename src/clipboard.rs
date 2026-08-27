#[derive(Default)]
pub struct Clipboard {
    pub text: String,
}

impl Clipboard {
    pub fn set(&mut self, text: String) {
        self.text = text;
    }

    pub fn get(&self) -> &str {
        &self.text
    }

    pub fn is_empty(&self) -> bool {
        self.text.is_empty()
    }
}