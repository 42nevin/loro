use super::*;
use super::json::*;
use crate::encoding::value::{OwnedValue, MarkStart};
#[test]
fn native_value_visitor_covers_existing_operand_families_without_metadata_changes() {
    let nan=||LoroValue::Double(f64::from_bits(0x7ff8_0000_0000_0042));
    let future=|value|JsonOpContent::Future(FutureOpWrapper{prop:7,value:FutureOp::Unknown(value)});
    let contents=vec![
        JsonOpContent::List(ListOp::Insert{pos:0,value:vec![nan(),nan()]}),
        JsonOpContent::MovableList(MovableListOp::Insert{pos:0,value:vec![nan(),nan()]}),
        JsonOpContent::MovableList(MovableListOp::Set{elem_id:IdLp::new(9,0),value:nan()}),
        JsonOpContent::Map(MapOp::Insert{key:"value".into(),value:nan()}),
        JsonOpContent::Text(TextOp::Mark{start:0,end:1,style_key:"style".into(),style_value:nan(),info:0}),
        future(OwnedValue::F64(f64::from_bits(0xfff8_0000_0000_0043))),
        future(OwnedValue::LoroValue(nan())),
        future(OwnedValue::MarkStart(MarkStart{len:1,key:"style".into(),value:nan(),info:0})),
        future(OwnedValue::ListSet{peer_idx:0,lamport:1,value:nan()}),
        JsonOpContent::Map(MapOp::Delete{key:"absent".into()}),
        JsonOpContent::Text(TextOp::Insert{pos:0,text:"literal玉".into()}),
    ];
    let container=ContainerID::new_root("fixture",ContainerType::Map);
    let mut schema=JsonSchema{schema_version:1,start_version:Frontiers::default(),peers:None,changes:vec![JsonChange{id:ID::new(9,0),timestamp:17,deps:vec![],lamport:0,msg:Some("original".into()),ops:contents.into_iter().enumerate().map(|(i,content)|JsonOp{counter:i as i32*3,container:container.clone(),content}).collect()}]};
    let mut seen=vec![];
    visit_native_values_mut(&mut schema,|change,operation,id,actual_container,field,value|->Result<(),()> {
        assert_eq!(change,0);assert_eq!(id,ID::new(9,operation as i32*3));assert_eq!(actual_container,&container);
        seen.push(field);
        match value {JsonNativeValueMut::Value(value)=>{match value {LoroValue::Double(v)=>assert_eq!(v.to_bits(),0x7ff8_0000_0000_0042),_=>panic!("kind lost before visitor")};*value=LoroValue::Null;},JsonNativeValueMut::Float(value)=>{assert_eq!(value.to_bits(),0xfff8_0000_0000_0043);*value=0.0;}}
        Ok(())
    }).unwrap();
    use JsonNativeValueField::*;
    assert_eq!(seen,vec![ListInsert(0),ListInsert(1),MovableInsert(0),MovableInsert(1),MovableSet,MapInsert,TextStyle,FutureFloat,FutureValue,FutureMark,FutureSet]);
    assert_eq!(schema.changes[0].timestamp,17);assert_eq!(schema.changes[0].msg.as_deref(),Some("original"));
    let mut count=0;let result=visit_native_values_mut(&mut schema,|_,_,_,_,_,_|->Result<(),&str>{count+=1;Err("stop")});assert_eq!(result,Err("stop"));assert_eq!(count,1);
}
