use std::collections::HashMap;

#[derive(Debug, Clone)]
pub struct TreeNode {
    pub name: Vec<u8>,
    pub apparent_size: u64,
    pub disk_usage: u64,
    pub file_count: u64,
    pub dir_count: u64,
    pub children: Vec<TreeNode>,
    pub is_dir: bool,
    pub depth: u16,
}

pub struct TreeBuilder {
    nodes: HashMap<Vec<u8>, TreeNode>,
    parents: HashMap<Vec<u8>, Vec<u8>>,
}

impl TreeBuilder {
    pub fn new() -> Self {
        Self {
            nodes: HashMap::new(),
            parents: HashMap::new(),
        }
    }

    pub fn insert(
        &mut self,
        path: &[u8],
        apparent_size: u64,
        disk_usage: u64,
        is_dir: bool,
        depth: u16,
    ) {
        let name = path.rsplit(|&b| b == b'/').next().unwrap_or(path).to_vec();
        let path_vec = path.to_vec();
        let parent = parent_path(path);

        self.nodes.insert(
            path_vec.clone(),
            TreeNode {
                name,
                apparent_size,
                disk_usage,
                file_count: if is_dir { 0 } else { 1 },
                dir_count: if is_dir { 1 } else { 0 },
                children: Vec::new(),
                is_dir,
                depth,
            },
        );

        if let Some(parent) = parent {
            self.parents.insert(path_vec, parent);
        }
    }

    pub fn build(mut self, roots: &[std::path::PathBuf]) -> Vec<TreeNode> {
        #[cfg(unix)]
        use std::os::unix::ffi::OsStrExt;

        let mut root_paths = Vec::new();
        for root in roots {
            #[cfg(unix)]
            let root_bytes = root.as_os_str().as_bytes().to_vec();
            #[cfg(not(unix))]
            let root_bytes = root.to_string_lossy().as_bytes().to_vec();

            let depth = root_bytes.iter().filter(|&&c| c == b'/').count() as u16;
            self.nodes
                .entry(root_bytes.clone())
                .or_insert_with(|| TreeNode {
                    name: root_bytes.clone(),
                    apparent_size: 0,
                    disk_usage: 0,
                    file_count: 0,
                    dir_count: 1,
                    children: Vec::new(),
                    is_dir: true,
                    depth,
                });
            root_paths.push(root_bytes);
        }

        let mut paths: Vec<Vec<u8>> = self.nodes.keys().cloned().collect();
        paths.sort_by(|a, b| {
            let depth_a = a.iter().filter(|&&c| c == b'/').count();
            let depth_b = b.iter().filter(|&&c| c == b'/').count();
            depth_b.cmp(&depth_a).then_with(|| a.cmp(b))
        });

        for path in &paths {
            if let Some(parent_path) = self.parents.get(path).cloned() {
                if let Some(node) = self.nodes.get(path) {
                    let apparent = node.apparent_size;
                    let disk = node.disk_usage;
                    let files = node.file_count;
                    let dirs = node.dir_count;

                    if let Some(parent) = self.nodes.get_mut(&parent_path) {
                        parent.apparent_size += apparent;
                        parent.disk_usage += disk;
                        parent.file_count += files;
                        parent.dir_count += dirs;
                    }
                }
            }
        }

        let mut children_by_parent: HashMap<Vec<u8>, Vec<Vec<u8>>> = HashMap::new();
        for (child, parent) in &self.parents {
            if self.nodes.contains_key(child) && self.nodes.contains_key(parent) {
                children_by_parent
                    .entry(parent.clone())
                    .or_default()
                    .push(child.clone());
            }
        }
        for children in children_by_parent.values_mut() {
            children.sort();
        }

        let mut roots_out = Vec::new();

        for root_bytes in root_paths {
            if let Some(mut root_node) =
                build_subtree(&root_bytes, &mut self.nodes, &children_by_parent)
            {
                root_node.name = root_bytes;
                roots_out.push(root_node);
            }
        }

        let mut leftovers: Vec<Vec<u8>> = self.nodes.keys().cloned().collect();
        leftovers.sort();
        for path in leftovers {
            if let Some(node) = build_subtree(&path, &mut self.nodes, &children_by_parent) {
                roots_out.push(node);
            }
        }

        roots_out
    }
}

fn build_subtree(
    path: &[u8],
    nodes: &mut HashMap<Vec<u8>, TreeNode>,
    children_by_parent: &HashMap<Vec<u8>, Vec<Vec<u8>>>,
) -> Option<TreeNode> {
    let mut node = nodes.remove(path)?;

    if let Some(children) = children_by_parent.get(path) {
        for child in children {
            if let Some(child_node) = build_subtree(child, nodes, children_by_parent) {
                node.children.push(child_node);
            }
        }
    }

    Some(node)
}

fn parent_path(path: &[u8]) -> Option<Vec<u8>> {
    path.iter()
        .rposition(|&b| b == b'/')
        .filter(|&pos| pos > 0)
        .map(|pos| path[..pos].to_vec())
}

impl Default for TreeBuilder {
    fn default() -> Self {
        Self::new()
    }
}
