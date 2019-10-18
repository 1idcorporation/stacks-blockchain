use vm::errors::{Error, InterpreterResult as Result, RuntimeErrorType};
use vm::analysis::errors::{CheckErrors};
use vm::types::{Value};
use vm::contexts::{OwnedEnvironment};
use vm::representations::SymbolicExpression;
use vm::database::marf::temporary_marf;
use vm::database::ClarityDatabase;
use vm::types::{QualifiedContractIdentifier, PrincipalData};

use vm::tests::{symbols_from_values, execute, is_err_code, is_committed};

use chainstate::stacks::index::storage::{TrieFileStorage};
use chainstate::burn::BlockHeaderHash;

const p1_str: &str = "'SZ2J6ZY48GV1EZ5V2V5RB9MP66SW86PYKKQ9H6DPR";

#[test]
fn test_forking_simple() {
    with_separate_forks_environment(
        initialize_contract,
        |x| { branched_execution(x, true); },
        |x| { branched_execution(x, true); },
        |x| { branched_execution(x, false); });
}

#[test]
fn test_at_block_good() {

    fn initialize(owned_env: &mut OwnedEnvironment) {
        let c = QualifiedContractIdentifier::local("contract").unwrap();
        let contract =
            "(define-data-var datum int 1)
             (define-public (reset)
               (begin
                 (var-set! datum (+
                   (at-block 0x0202020202020202020202020202020202020202020202020202020202020202 (var-get datum))
                   (at-block 0x0101010101010101010101010101010101010101010101010101010101010101 (var-get datum))))
                 (ok (var-get datum))))
             (define-public (set-val)
               (begin
                 (var-set! datum 10)
                 (ok (var-get datum))))";

        eprintln!("Initializing contract...");
        owned_env.initialize_contract(c.clone(), &contract).unwrap();
    }


    fn branch(owned_env: &mut OwnedEnvironment, expected_value: i128, to_exec: &str) -> Result<Value> {
        let c = QualifiedContractIdentifier::local("contract").unwrap();
        let p1 = execute(p1_str);
        eprintln!("Branched execution...");

        {
            let mut env = owned_env.get_exec_environment(None);
            let command = format!("(var-get datum)");
            let value = env.eval_read_only(&c, &command).unwrap();
            assert_eq!(value, Value::Int(expected_value));
        }
        
        owned_env.execute_transaction(p1, c, to_exec, &vec![])
            .map(|(x, _)| x)
    }

    with_separate_forks_environment(
        initialize,
        |x| {
            assert_eq!(branch(x, 1, "set-val").unwrap(),
                       Value::okay(Value::Int(10)));
        },
        |x| {
            let resp = branch(x, 1, "reset").unwrap_err();
            eprintln!("{}", resp);
            match resp {
                Error::Runtime(x, _) =>
                    assert_eq!(x, RuntimeErrorType::UnknownBlockHeaderHash(BlockHeaderHash::from(vec![2 as u8; 32].as_slice()))),
                _ => panic!("Unexpected error")
            }
        },
        |x| {
            assert_eq!(branch(x, 10, "reset").unwrap(),
                       Value::okay(Value::Int(11)));
        });
}

#[test]
fn test_at_block_missing_defines() {
    fn initialize_1(owned_env: &mut OwnedEnvironment) {
        let c_a = QualifiedContractIdentifier::local("contract-a").unwrap();
        let c_b = QualifiedContractIdentifier::local("contract-b").unwrap();

        let contract =
            "(define-map datum ((id bool)) ((value int)))
             
             (define-public (flip)
               (let ((current (default-to (get value (map-get! datum ((id 'true)))) 0)))
                 (map-set! datum ((id 'true)) (if (eq? 1 current) 0 1))
                 (ok current)))";

        eprintln!("Initializing contract...");
        owned_env.initialize_contract(c_a.clone(), &contract).unwrap();
    }

    fn initialize_2(owned_env: &mut OwnedEnvironment) -> Error {
        let c_a = QualifiedContractIdentifier::local("contract-a").unwrap();
        let c_b = QualifiedContractIdentifier::local("contract-b").unwrap();

        let contract =
            "(define-private (problematic-cc)
               (at-block 0x0101010101010101010101010101010101010101010101010101010101010101
                 (contract-call! .contract-a flip)))
             (problematic-cc)
            ";

        eprintln!("Initializing contract...");
        let e = owned_env.initialize_contract(c_b.clone(), &contract).unwrap_err();
        e
    }

    fn initialize_3(owned_env: &mut OwnedEnvironment) -> Error {
        let c_a = QualifiedContractIdentifier::local("contract-a").unwrap();
        let c_b = QualifiedContractIdentifier::local("contract-b").unwrap();

        let contract =
            "(define-private (problematic-fetch-entry)
               (at-block 0x0101010101010101010101010101010101010101010101010101010101010101
                 (contract-map-get .contract-a datum ((id 'true)))))
             (problematic-fetch-entry)
            ";

        eprintln!("Initializing contract...");
        let e = owned_env.initialize_contract(c_b.clone(), &contract).unwrap_err();
        e
    }

    with_separate_forks_environment(
        |_| {},
        initialize_1,
        |_| {},
        |env| {
            let err = initialize_2(env);
            assert_eq!(err, CheckErrors::NoSuchContract("'S1G2081040G2081040G2081040G208105NK8PE5.contract-a".into()).into());
        });

    with_separate_forks_environment(
        |_| {},
        initialize_1,
        |_| {},
        |env| {
            let err = initialize_3(env);
            assert_eq!(err, CheckErrors::NoSuchMap("datum".into()).into());
        });

}

// execute:
// f -> a -> z
//    \--> b
// with f @ block 1;32
// with a @ block 2;32
// with b @ block 3;32
// with z @ block 4;32

fn with_separate_forks_environment<F0, F1, F2, F3>(f: F0, a: F1, b: F2, z: F3)
where F0: FnOnce(&mut OwnedEnvironment),
      F1: FnOnce(&mut OwnedEnvironment),
      F2: FnOnce(&mut OwnedEnvironment),
      F3: FnOnce(&mut OwnedEnvironment)
{
    let mut marf_kv = temporary_marf();
    marf_kv.begin(&TrieFileStorage::block_sentinel(),
                  &BlockHeaderHash::from_bytes(&[0 as u8; 32]).unwrap());

    {
        let mut clarity_db = ClarityDatabase::new(Box::new(&mut marf_kv));
        clarity_db.initialize();
    }

    marf_kv.commit();
    marf_kv.begin(&BlockHeaderHash::from_bytes(&[0 as u8; 32]).unwrap(),
                  &BlockHeaderHash::from_bytes(&[1 as u8; 32]).unwrap());

    {
        let clarity_db = ClarityDatabase::new(Box::new(&mut marf_kv));
        let mut owned_env = OwnedEnvironment::new(clarity_db);
        f(&mut owned_env)
    }

    marf_kv.commit();

    // Now, we can do our forking.

    marf_kv.begin(&BlockHeaderHash::from_bytes(&[1 as u8; 32]).unwrap(),
                  &BlockHeaderHash::from_bytes(&[2 as u8; 32]).unwrap());

    {
        let clarity_db = ClarityDatabase::new(Box::new(&mut marf_kv));
        let mut owned_env = OwnedEnvironment::new(clarity_db);
        a(&mut owned_env)
    }

    marf_kv.commit();

    marf_kv.begin(&BlockHeaderHash::from_bytes(&[1 as u8; 32]).unwrap(),
                  &BlockHeaderHash::from_bytes(&[3 as u8; 32]).unwrap());

    {
        let clarity_db = ClarityDatabase::new(Box::new(&mut marf_kv));
        let mut owned_env = OwnedEnvironment::new(clarity_db);
        b(&mut owned_env)
    }

    marf_kv.commit();


    marf_kv.begin(&BlockHeaderHash::from_bytes(&[2 as u8; 32]).unwrap(),
                  &BlockHeaderHash::from_bytes(&[4 as u8; 32]).unwrap());

    {
        let clarity_db = ClarityDatabase::new(Box::new(&mut marf_kv));
        let mut owned_env = OwnedEnvironment::new(clarity_db);
        z(&mut owned_env)
    }

    marf_kv.commit();
    
}

fn initialize_contract(owned_env: &mut OwnedEnvironment) {
    let p1_address = {
        if let Value::Principal(PrincipalData::Standard(address)) = execute(p1_str) {
            address
        } else {
            panic!();
        }
    };
    let contract = format!(
        "(define-constant burn-address 'SP000000000000000000002Q6VF78)
         (define-fungible-token stackaroos)
         (define-read-only (get-balance (p principal))
           (ft-get-balance stackaroos p))
         (define-public (destroy (x int))
           (if (< (ft-get-balance stackaroos tx-sender) x)
               (err -1)
               (ft-transfer! stackaroos x tx-sender burn-address)))
         (ft-mint! stackaroos 10 {})", p1_str);

    eprintln!("Initializing contract...");

    let contract_identifier = QualifiedContractIdentifier::new(p1_address, "tokens".into());
    owned_env.initialize_contract(contract_identifier, &contract).unwrap();
}

fn branched_execution(owned_env: &mut OwnedEnvironment, expect_success: bool) {
    let p1_address = {
        if let Value::Principal(PrincipalData::Standard(address)) = execute(p1_str) {
            address
        } else {
            panic!();
        }
    };
    let contract_identifier = QualifiedContractIdentifier::new(p1_address.clone(), "tokens".into());

    eprintln!("Branched execution...");

    {
        let mut env = owned_env.get_exec_environment(None);
        let command = format!("(get-balance {})", p1_str);
        let balance = env.eval_read_only(&contract_identifier, 
                                         &command).unwrap();
        let expected = if expect_success {
            10
        } else {
            0
        };
        assert_eq!(balance, Value::Int(expected));
    }

    let (result, _) = owned_env.execute_transaction(Value::Principal(PrincipalData::Standard(p1_address)),
                                                    contract_identifier, 
                                                    "destroy",
                                                    &symbols_from_values(vec![Value::Int(10)])).unwrap();

    if expect_success {
        assert!(is_committed(&result))
    } else {
        assert!(is_err_code(&result, -1))
    }
}

