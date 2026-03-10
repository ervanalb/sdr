#[derive(Default)]
pub struct IdFactory(usize);

impl IdFactory {
    pub fn create(&mut self) -> usize {
        let id = self.0;
        self.0 += 1;
        id
    }
}
