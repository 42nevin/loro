// Independent unchanged-reader oracle. Only this example is added to the exact
// 531a959f5322f9fd42f30b5de65f1047a721b498 source snapshot.
use loro::{IdSpan, LoroDoc};
fn main() {
    let doc = LoroDoc::new();
    let mut statuses = Vec::new();
    for path in std::env::args().skip(1) {
        let bytes = std::fs::read(path).unwrap();
        match doc.import(&bytes) {
            Ok(status) => statuses.push(format!("{status:?}")),
            Err(error) => {
                println!("{}", serde_json::json!({"error": format!("{error:?}")}));
                std::process::exit(2);
            }
        }
    }
    let mut atoms = Vec::new();
    let mut peers: Vec<_> = doc.oplog_vv().iter().map(|(&p, &c)| (p, c)).collect();
    peers.sort_unstable();
    for (peer, end) in &peers {
        for counter in 0..*end {
            let mut changes = doc.export_json_in_id_span(IdSpan::new(*peer, counter, counter + 1));
            for change in &mut changes {
                change.deps.sort_unstable_by_key(|id| (id.peer, id.counter));
                for op in &mut change.ops {
                    match &mut op.content {
                        loro::JsonOpContent::Text(loro::JsonTextOp::Delete { len, .. })
                        | loro::JsonOpContent::List(loro::JsonListOp::Delete { len, .. })
                        | loro::JsonOpContent::MovableList(loro::JsonMovableListOp::Delete {
                            len,
                            ..
                        }) => {
                            if *len == -1 {
                                *len = 1;
                            }
                        }
                        _ => {}
                    }
                }
            }
            atoms.push(serde_json::to_value(changes).unwrap());
        }
    }
    let mut frontiers: Vec<_> = doc
        .oplog_frontiers()
        .iter()
        .map(|id| (id.peer, id.counter))
        .collect();
    frontiers.sort_unstable();
    println!(
        "{}",
        serde_json::json!({"state": doc.get_deep_value(), "peers": peers,
        "frontiers": frontiers, "atoms": atoms})
    );
}
