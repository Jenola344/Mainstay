#![cfg(test)]

use lending::{LendingContract, LendingContractClient};
use soroban_sdk::{testutils::Address as _, token, Address, Env};

fn setup_contract(env: &Env) -> (LendingContractClient, Address, Address) {
    let contract_id = env.register(LendingContract, ());
    let client = LendingContractClient::new(env, &contract_id);

    let deployer = Address::generate(env);
    let admin = Address::generate(env);
    let token_admin = Address::generate(env);

    let token_id = env.register_stellar_asset_contract(token_admin.clone());

    env.mock_all_auths();
    client.initialize(&deployer, &admin, &token_id);

    (client, token_id, contract_id)
}

#[test]
fn test_get_contract_balance_zero() {
    let env = Env::default();
    let (client, _token_id, _contract_id) = setup_contract(&env);

    let balance = client.get_contract_balance();
    assert_eq!(balance, 0);
}

#[test]
fn test_get_contract_balance_with_funds() {
    let env = Env::default();
    let (client, token_id, contract_id) = setup_contract(&env);

    let token_admin = Address::generate(&env);
    let token_client = token::Client::new(&env, &token_id);

    token_client.mint(&token_admin, &contract_id, &1000);

    let balance = client.get_contract_balance();
    assert_eq!(balance, 1000);
}

#[test]
fn test_get_contract_balance_after_transfer() {
    let env = Env::default();
    let (client, token_id, contract_id) = setup_contract(&env);

    let token_admin = Address::generate(&env);
    let token_client = token::Client::new(&env, &token_id);

    token_client.mint(&token_admin, &contract_id, &5000);
    assert_eq!(client.get_contract_balance(), 5000);

    let recipient = Address::generate(&env);
    token_client.mint(&token_admin, &recipient, &100);
    token_client.transfer(&contract_id, &recipient, &1000);

    assert_eq!(client.get_contract_balance(), 4000);
}
