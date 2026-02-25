#[derive(Debug)]
pub struct TreeNode {
    pub name: Vec<u8>,
    pub size: u64,
    pub children: Vec<TreeNode>,
    pub is_dir: bool,
    pub depth: u16,
}

pub struct TreeBuilder;

impl TreeBuilder {
    pub fn new() -> Self {
        Self
    }
}

impl Default for TreeBuilder {
    fn default() -> Self {
        Self::new()
    }
}
