use hokori_scan::ScanResult;
use serde::Serialize;

#[derive(Serialize)]
struct JsonOutput {
    total_size: u64,
    file_count: u64,
    dir_count: u64,
    error_count: u64,
    deduped_count: u64,
    roots: Vec<JsonRoot>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tree: Option<Vec<JsonTreeNode>>,
}

#[derive(Serialize)]
struct JsonRoot {
    path: String,
    total_size: u64,
    file_count: u64,
    dir_count: u64,
}

#[derive(Serialize)]
struct JsonTreeNode {
    name: String,
    apparent_size: u64,
    disk_usage: u64,
    file_count: u64,
    dir_count: u64,
    is_dir: bool,
    depth: u16,
    children: Vec<JsonTreeNode>,
}

pub fn render(result: &ScanResult, _errors: &[impl std::fmt::Display]) {
    let output = JsonOutput {
        total_size: result.total_size,
        file_count: result.file_count,
        dir_count: result.dir_count,
        error_count: result.error_count,
        deduped_count: result.deduped_count,
        roots: result
            .roots
            .iter()
            .map(|r| JsonRoot {
                path: r.path.display().to_string(),
                total_size: r.total_size,
                file_count: r.file_count,
                dir_count: r.dir_count,
            })
            .collect(),
        tree: result.tree.as_ref().map(|tree| {
            tree.root_indices
                .iter()
                .map(|&idx| tree_to_json_nodes(tree, idx))
                .collect()
        }),
    };
    println!("{}", serde_json::to_string(&output).unwrap());
}

fn tree_to_json_nodes(tree: &hokori_scan::tree::BuiltTree, idx: u32) -> JsonTreeNode {
    let node = &tree.nodes[idx as usize];
    JsonTreeNode {
        name: String::from_utf8_lossy(&node.name).into_owned(),
        apparent_size: node.apparent_size,
        disk_usage: node.disk_usage,
        file_count: node.file_count,
        dir_count: node.dir_count,
        is_dir: node.is_dir,
        depth: node.depth,
        children: tree
            .children(idx)
            .map(|(child_idx, _)| tree_to_json_nodes(tree, child_idx))
            .collect(),
    }
}
