use hokori_scan::ScanResult;
use hokori_scan::tree::TreeNode;
use serde_json::{Map, Value, json};

pub fn render(result: &ScanResult, roots: &[std::path::PathBuf]) {
    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();

    let mut output = vec![
        json!(1),
        json!(2),
        json!({
            "progname": "hokori",
            "progver": env!("CARGO_PKG_VERSION"),
            "timestamp": timestamp,
        }),
    ];

    if let Some(tree) = &result.tree {
        for root_node in tree {
            output.push(node_to_value(root_node));
        }
    } else {
        for root in roots {
            output.push(Value::Array(vec![node_info(
                &root.display().to_string(),
                result.total_size,
                result.total_size,
            )]));
        }
        eprintln!("note: full ncdu tree export requires --build-tree");
    }

    println!("{}", serde_json::to_string(&Value::Array(output)).unwrap());
}

fn node_to_value(node: &TreeNode) -> Value {
    let name = String::from_utf8_lossy(&node.name).into_owned();
    let info = node_info(&name, node.apparent_size, node.disk_usage);

    if node.is_dir {
        let mut items = vec![info];
        for child in &node.children {
            items.push(node_to_value(child));
        }
        Value::Array(items)
    } else {
        info
    }
}

fn node_info(name: &str, apparent_size: u64, disk_usage: u64) -> Value {
    let mut info = Map::new();
    info.insert("name".to_string(), Value::String(name.to_string()));
    info.insert("asize".to_string(), Value::Number(apparent_size.into()));
    info.insert("dsize".to_string(), Value::Number(disk_usage.into()));
    Value::Object(info)
}
