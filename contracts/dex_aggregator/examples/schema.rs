use std::env::current_dir;
use std::fs::create_dir_all;

use cosmwasm_schema::{export_schema, export_schema_with_title, remove_schemas, schema_for};

use dex_aggregator::msg::{
    AllFeesResponse, ExecuteMsg, FeeResponse, InstantiateMsg, QueryMsg, SimulateRouteResponse,
};
use dex_aggregator::state::Config;

fn main() {
    let mut out_dir = current_dir().unwrap();
    out_dir.push("schema");
    create_dir_all(&out_dir).unwrap();
    remove_schemas(&out_dir).unwrap();

    export_schema(&schema_for!(InstantiateMsg), &out_dir);
    export_schema_with_title(&schema_for!(ExecuteMsg), &out_dir, "ExecuteMsg");
    export_schema_with_title(&schema_for!(QueryMsg), &out_dir, "QueryMsg");
    export_schema(&schema_for!(SimulateRouteResponse), &out_dir);
    export_schema(&schema_for!(Config), &out_dir); 
    export_schema(&schema_for!(FeeResponse), &out_dir);
    export_schema(&schema_for!(AllFeesResponse), &out_dir);

    println!("JSON schemas generated to the an aggregated directory: ./schema");
}