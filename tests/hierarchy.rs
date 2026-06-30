use ktlsp::workspace::Workspace;

#[test]
fn implementation_finds_direct_subtypes() {
    let mut ws = Workspace::new();
    let key = "mem:///Main.kt".to_string();
    let src = "package app\ninterface Greeter\nclass ConsoleGreeter : Greeter\n";
    ws.open(key.clone(), src.to_string());

    let defs = ws.implementation(&key, src.find("Greeter").unwrap());

    assert_eq!(defs.len(), 1, "{defs:?}");
    assert_eq!(&src[defs[0].start_byte..defs[0].end_byte], "ConsoleGreeter");
}

#[test]
fn type_definition_uses_inferred_constructor_type() {
    let mut ws = Workspace::new();
    let key = "mem:///Main.kt".to_string();
    let src = "package app\nclass GreeterImpl\nfun main() {\n    val g = GreeterImpl()\n    println(g)\n}\n";
    ws.open(key.clone(), src.to_string());

    let defs = ws.type_definition(&key, src.rfind("g)").unwrap());

    assert_eq!(defs.len(), 1, "{defs:?}");
    assert_eq!(&src[defs[0].start_byte..defs[0].end_byte], "GreeterImpl");
}

#[test]
fn call_hierarchy_reports_incoming_and_outgoing_calls() {
    let mut ws = Workspace::new();
    let key = "mem:///Main.kt".to_string();
    let src = "package app\nfun helper(): Int = 1\nfun caller(): Int = helper()\n";
    ws.open(key.clone(), src.to_string());

    let helper = ws
        .hierarchy_item_at(&key, src.find("helper").unwrap())
        .expect("helper hierarchy item");
    let incoming = ws.incoming_calls(&helper);
    assert_eq!(incoming.len(), 1, "{incoming:?}");
    assert_eq!(incoming[0].from.name, "caller");

    let caller = ws
        .hierarchy_item_at(&key, src.find("caller").unwrap())
        .expect("caller hierarchy item");
    let outgoing = ws.outgoing_calls(&caller);
    assert_eq!(outgoing.len(), 1, "{outgoing:?}");
    assert_eq!(outgoing[0].to.name, "helper");
}

#[test]
fn type_hierarchy_returns_direct_neighbors() {
    let mut ws = Workspace::new();
    let key = "mem:///Main.kt".to_string();
    let src = "package app\nopen class Base\nclass Child : Base\n";
    ws.open(key.clone(), src.to_string());

    let base = ws
        .hierarchy_item_at(&key, src.find("Base").unwrap())
        .expect("base item");
    let child = ws
        .hierarchy_item_at(&key, src.find("Child").unwrap())
        .expect("child item");

    assert_eq!(ws.type_subtypes(&base)[0].name, "Child");
    assert_eq!(ws.type_supertypes(&child)[0].name, "Base");
}
