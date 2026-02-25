use std::collections::HashMap;

pub type NodeIdx = u32;
const NONE: NodeIdx = u32::MAX;

#[derive(Debug, Clone)]
pub struct TreeNode {
    pub name: Vec<u8>,
    pub apparent_size: u64,
    pub disk_usage: u64,
    pub file_count: u64,
    pub dir_count: u64,
    pub is_dir: bool,
    pub depth: u16,
    parent: NodeIdx,
    first_child: NodeIdx,
    next_sibling: NodeIdx,
}

pub struct TreeBuilder {
    nodes: Vec<TreeNode>,
    path_to_idx: HashMap<Vec<u8>, NodeIdx>,
    paths: Vec<Vec<u8>>,
}

impl TreeBuilder {
    pub fn new() -> Self {
        Self {
            nodes: Vec::new(),
            path_to_idx: HashMap::new(),
            paths: Vec::new(),
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
        let idx = self.nodes.len() as NodeIdx;
        let path_vec = path.to_vec();
        let name = path.rsplit(|&b| b == b'/').next().unwrap_or(path).to_vec();

        self.nodes.push(TreeNode {
            name,
            apparent_size,
            disk_usage,
            file_count: if is_dir { 0 } else { 1 },
            dir_count: if is_dir { 1 } else { 0 },
            is_dir,
            depth,
            parent: NONE,
            first_child: NONE,
            next_sibling: NONE,
        });

        self.path_to_idx.insert(path_vec.clone(), idx);
        self.paths.push(path_vec);
    }

    pub fn build(mut self, roots: &[std::path::PathBuf]) -> BuiltTree {
        #[cfg(unix)]
        use std::os::unix::ffi::OsStrExt;

        let mut root_paths = Vec::new();
        for root in roots {
            #[cfg(unix)]
            let root_bytes = root.as_os_str().as_bytes().to_vec();
            #[cfg(not(unix))]
            let root_bytes = root.to_string_lossy().as_bytes().to_vec();

            root_paths.push(root_bytes.clone());

            if !self.path_to_idx.contains_key(&root_bytes) {
                let idx = self.nodes.len() as NodeIdx;
                let depth = root_bytes.iter().filter(|&&c| c == b'/').count() as u16;
                self.nodes.push(TreeNode {
                    name: root_bytes.clone(),
                    apparent_size: 0,
                    disk_usage: 0,
                    file_count: 0,
                    dir_count: 1,
                    is_dir: true,
                    depth,
                    parent: NONE,
                    first_child: NONE,
                    next_sibling: NONE,
                });
                self.path_to_idx.insert(root_bytes.clone(), idx);
                self.paths.push(root_bytes);
            }
        }

        for idx in 0..self.nodes.len() as NodeIdx {
            let path = &self.paths[idx as usize];
            if let Some(parent_path) = parent_path(path) {
                if let Some(&parent_idx) = self.path_to_idx.get(&parent_path) {
                    if parent_idx != idx {
                        self.nodes[idx as usize].parent = parent_idx;
                        let old_first = self.nodes[parent_idx as usize].first_child;
                        self.nodes[idx as usize].next_sibling = old_first;
                        self.nodes[parent_idx as usize].first_child = idx;
                    }
                }
            }
        }

        let mut indices: Vec<NodeIdx> = (0..self.nodes.len() as NodeIdx).collect();
        indices.sort_unstable_by(|&a, &b| self.nodes[b as usize].depth.cmp(&self.nodes[a as usize].depth));

        for idx in indices {
            let parent = self.nodes[idx as usize].parent;
            if parent != NONE {
                let apparent = self.nodes[idx as usize].apparent_size;
                let disk = self.nodes[idx as usize].disk_usage;
                let files = self.nodes[idx as usize].file_count;
                let dirs = self.nodes[idx as usize].dir_count;
                self.nodes[parent as usize].apparent_size += apparent;
                self.nodes[parent as usize].disk_usage += disk;
                self.nodes[parent as usize].file_count += files;
                self.nodes[parent as usize].dir_count += dirs;
            }
        }

        let mut root_indices = Vec::new();
        for root_path in &root_paths {
            if let Some(&idx) = self.path_to_idx.get(root_path) {
                self.nodes[idx as usize].name = root_path.clone();
                root_indices.push(idx);
            }
        }

        if root_indices.is_empty() {
            for idx in 0..self.nodes.len() as NodeIdx {
                if self.nodes[idx as usize].parent == NONE {
                    root_indices.push(idx);
                }
            }
        }

        BuiltTree {
            nodes: self.nodes,
            root_indices,
        }
    }
}

fn parent_path(path: &[u8]) -> Option<Vec<u8>> {
    path.iter()
        .rposition(|&b| b == b'/')
        .filter(|&pos| pos > 0)
        .map(|pos| path[..pos].to_vec())
}

#[derive(Debug, Clone)]
pub struct BuiltTree {
    pub nodes: Vec<TreeNode>,
    pub root_indices: Vec<NodeIdx>,
}

impl BuiltTree {
    pub fn children(&self, idx: NodeIdx) -> ChildIter<'_> {
        ChildIter {
            tree: self,
            current: self.nodes[idx as usize].first_child,
        }
    }

    pub fn top_dirs(&self, n: usize, use_apparent: bool) -> Vec<(u64, String)> {
        let mut dirs = Vec::new();
        for &root_idx in &self.root_indices {
            let mut root_name = String::from_utf8_lossy(&self.nodes[root_idx as usize].name).into_owned();
            if root_name.is_empty() {
                root_name = "/".to_string();
            }
            self.collect_dirs(root_idx, &root_name, use_apparent, false, &mut dirs);
        }
        dirs.sort_unstable_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
        dirs.truncate(n);
        dirs
    }

    fn collect_dirs(
        &self,
        idx: NodeIdx,
        path: &str,
        use_apparent: bool,
        include_self: bool,
        out: &mut Vec<(u64, String)>,
    ) {
        let node = &self.nodes[idx as usize];
        if !node.is_dir {
            return;
        }

        if include_self {
            let size = if use_apparent {
                node.apparent_size
            } else {
                node.disk_usage
            };
            out.push((size, path.to_string()));
        }

        let mut child = node.first_child;
        while child != NONE {
            let child_name = String::from_utf8_lossy(&self.nodes[child as usize].name);
            let child_path = if path.ends_with('/') {
                format!("{path}{child_name}")
            } else {
                format!("{path}/{child_name}")
            };
            self.collect_dirs(child, &child_path, use_apparent, true, out);
            child = self.nodes[child as usize].next_sibling;
        }
    }

    pub fn write_ncdu_node<W: std::io::Write>(&self, w: &mut W, idx: NodeIdx) -> std::io::Result<()> {
        let node = &self.nodes[idx as usize];
        let name = String::from_utf8_lossy(&node.name);

        if node.is_dir {
            write!(w, "[{{\"name\":")?;
            write_json_string(w, &name)?;
            write!(w, ",\"asize\":{},\"dsize\":{}", node.apparent_size, node.disk_usage)?;
            write!(w, "}}")?;
            let mut child = node.first_child;
            while child != NONE {
                write!(w, ",")?;
                self.write_ncdu_node(w, child)?;
                child = self.nodes[child as usize].next_sibling;
            }
            write!(w, "]")?;
        } else {
            write!(w, "{{\"name\":")?;
            write_json_string(w, &name)?;
            write!(w, ",\"asize\":{},\"dsize\":{}", node.apparent_size, node.disk_usage)?;
            write!(w, "}}")?;
        }

        Ok(())
    }
}

pub struct ChildIter<'a> {
    tree: &'a BuiltTree,
    current: NodeIdx,
}

impl<'a> Iterator for ChildIter<'a> {
    type Item = (NodeIdx, &'a TreeNode);

    fn next(&mut self) -> Option<Self::Item> {
        if self.current == NONE {
            return None;
        }

        let idx = self.current;
        let node = &self.tree.nodes[idx as usize];
        self.current = node.next_sibling;
        Some((idx, node))
    }
}

impl Default for TreeBuilder {
    fn default() -> Self {
        Self::new()
    }
}

fn write_json_string<W: std::io::Write>(w: &mut W, s: &str) -> std::io::Result<()> {
    write!(w, "\"")?;
    for ch in s.chars() {
        match ch {
            '"' => write!(w, "\\\"")?,
            '\\' => write!(w, "\\\\")?,
            '\u{08}' => write!(w, "\\b")?,
            '\u{0C}' => write!(w, "\\f")?,
            '\n' => write!(w, "\\n")?,
            '\r' => write!(w, "\\r")?,
            '\t' => write!(w, "\\t")?,
            c if c <= '\u{1F}' => write!(w, "\\u{:04x}", c as u32)?,
            c => write!(w, "{c}")?,
        }
    }
    write!(w, "\"")
}
