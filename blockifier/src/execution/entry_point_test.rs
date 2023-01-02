use pretty_assertions::assert_eq;
use starknet_api::core::{ClassHash, ContractAddress, EntryPointSelector};
use starknet_api::hash::StarkHash;
use starknet_api::shash;
use starknet_api::state::EntryPointType;
use starknet_api::transaction::Calldata;

use crate::abi::abi_utils::get_selector_from_name;
use crate::execution::entry_point::{CallEntryPoint, CallExecution, CallInfo, Retdata};
use crate::retdata;
use crate::state::cached_state::{CachedState, DictStateReader};
use crate::state::state_api::State;
use crate::test_utils::{
    create_security_test_state, create_test_state, BITWISE_AND_SELECTOR, RETURN_RESULT_SELECTOR,
    SQRT_SELECTOR, TEST_CALL_CONTRACT_SELECTOR, TEST_CLASS_HASH, TEST_CONTRACT_ADDRESS,
    TEST_DEPLOY_SELECTOR, TEST_LIBRARY_CALL_SELECTOR, TEST_NESTED_LIBRARY_CALL_SELECTOR,
    TEST_STORAGE_READ_WRITE_SELECTOR, TEST_STORAGE_VAR_SELECTOR, WITHOUT_ARG_SELECTOR,
    WITH_ARG_SELECTOR,
};

fn trivial_external_entry_point() -> CallEntryPoint {
    CallEntryPoint {
        class_hash: None,
        entry_point_type: EntryPointType::External,
        entry_point_selector: EntryPointSelector(shash!(0)),
        calldata: Calldata(vec![].into()),
        storage_address: ContractAddress::try_from(shash!(TEST_CONTRACT_ADDRESS)).unwrap(),
        caller_address: ContractAddress::default(),
    }
}

#[test]
fn test_call_info() {
    let mut state = create_test_state();
    let entry_point_call = CallEntryPoint {
        entry_point_selector: EntryPointSelector(shash!(WITHOUT_ARG_SELECTOR)),
        ..trivial_external_entry_point()
    };
    let expected_call_info = CallInfo {
        call: entry_point_call.clone(),
        execution: CallExecution { retdata: retdata![] },
        ..Default::default()
    };
    assert_eq!(entry_point_call.execute_directly(&mut state).unwrap(), expected_call_info);
}

#[test]
fn test_entry_point_without_arg() {
    let mut state = create_test_state();
    let entry_point_call = CallEntryPoint {
        entry_point_selector: EntryPointSelector(shash!(WITHOUT_ARG_SELECTOR)),
        ..trivial_external_entry_point()
    };
    assert_eq!(
        entry_point_call.execute_directly(&mut state).unwrap().execution,
        CallExecution { retdata: retdata![] }
    );
}

#[test]
fn test_entry_point_with_arg() {
    let mut state = create_test_state();
    let calldata = Calldata(vec![shash!(25)].into());
    let entry_point_call = CallEntryPoint {
        calldata,
        entry_point_selector: EntryPointSelector(shash!(WITH_ARG_SELECTOR)),
        ..trivial_external_entry_point()
    };
    assert_eq!(
        entry_point_call.execute_directly(&mut state).unwrap().execution,
        CallExecution { retdata: retdata![] }
    );
}

#[test]
fn test_entry_point_with_builtin() {
    let mut state = create_test_state();
    let calldata = Calldata(vec![shash!(47), shash!(31)].into());
    let entry_point_call = CallEntryPoint {
        calldata,
        entry_point_selector: EntryPointSelector(shash!(BITWISE_AND_SELECTOR)),
        ..trivial_external_entry_point()
    };
    assert_eq!(
        entry_point_call.execute_directly(&mut state).unwrap().execution,
        CallExecution { retdata: retdata![] }
    );
}

#[test]
fn test_entry_point_with_hint() {
    let mut state = create_test_state();
    let calldata = Calldata(vec![shash!(81)].into());
    let entry_point_call = CallEntryPoint {
        calldata,
        entry_point_selector: EntryPointSelector(shash!(SQRT_SELECTOR)),
        ..trivial_external_entry_point()
    };
    assert_eq!(
        entry_point_call.execute_directly(&mut state).unwrap().execution,
        CallExecution { retdata: retdata![] }
    );
}

#[test]
fn test_entry_point_with_return_value() {
    let mut state = create_test_state();
    let calldata = Calldata(vec![shash!(23)].into());
    let entry_point_call = CallEntryPoint {
        calldata,
        entry_point_selector: EntryPointSelector(shash!(RETURN_RESULT_SELECTOR)),
        ..trivial_external_entry_point()
    };
    assert_eq!(
        entry_point_call.execute_directly(&mut state).unwrap().execution,
        CallExecution { retdata: retdata![shash!(23)] }
    );
}

#[test]
fn test_entry_point_not_found_in_contract() {
    let mut state = create_test_state();
    let entry_point_call = CallEntryPoint {
        entry_point_selector: EntryPointSelector(shash!(2)),
        ..trivial_external_entry_point()
    };
    assert_eq!(
        format!("Entry point {:#?} not found in contract.", entry_point_call.entry_point_selector),
        format!("{}", entry_point_call.execute_directly(&mut state).unwrap_err())
    );
}

#[test]
fn test_entry_point_with_syscall() {
    let mut state = create_test_state();
    let key = shash!(1234);
    let value = shash!(18);
    let calldata = Calldata(vec![key, value].into());
    let entry_point_call = CallEntryPoint {
        calldata,
        entry_point_selector: EntryPointSelector(shash!(TEST_STORAGE_READ_WRITE_SELECTOR)),
        ..trivial_external_entry_point()
    };
    let storage_address = entry_point_call.storage_address;
    assert_eq!(
        entry_point_call.execute_directly(&mut state).unwrap().execution,
        CallExecution { retdata: retdata![shash!(value)] }
    );
    // Verify that the state has changed.
    let value_from_state = *state.get_storage_at(storage_address, key.try_into().unwrap()).unwrap();
    assert_eq!(value_from_state, value);
}

#[test]
fn test_entry_point_with_library_call() {
    let mut state = create_test_state();
    let calldata = Calldata(
        vec![
            shash!(TEST_CLASS_HASH),                  // Class hash.
            shash!(TEST_STORAGE_READ_WRITE_SELECTOR), // Function selector.
            shash!(2),                                // Calldata length.
            shash!(1234),                             // Calldata: address.
            shash!(91),                               // Calldata: value.
        ]
        .into(),
    );
    let entry_point_call = CallEntryPoint {
        entry_point_selector: EntryPointSelector(shash!(TEST_LIBRARY_CALL_SELECTOR)),
        calldata,
        class_hash: Some(ClassHash(shash!(TEST_CLASS_HASH))),
        ..trivial_external_entry_point()
    };
    assert_eq!(
        entry_point_call.execute_directly(&mut state).unwrap().execution,
        CallExecution { retdata: retdata![shash!(91)] }
    );
}

#[test]
fn test_entry_point_with_nested_library_call() {
    let mut state = create_test_state();
    let (key, value) = (255, 44);
    let calldata = Calldata(
        vec![
            shash!(TEST_CLASS_HASH),                  // Class hash.
            shash!(TEST_LIBRARY_CALL_SELECTOR),       // Library call function selector.
            shash!(TEST_STORAGE_READ_WRITE_SELECTOR), // Storage function selector.
            shash!(2),                                // Calldata length.
            shash!(key),                              // Calldata: address.
            shash!(value),                            // Calldata: value.
        ]
        .into(),
    );

    // Create expected call info tree.
    let main_entry_point = CallEntryPoint {
        entry_point_selector: EntryPointSelector(shash!(TEST_NESTED_LIBRARY_CALL_SELECTOR)),
        calldata,
        class_hash: Some(ClassHash(shash!(TEST_CLASS_HASH))),
        ..trivial_external_entry_point()
    };
    let nested_storage_entry_point = CallEntryPoint {
        entry_point_selector: EntryPointSelector(shash!(TEST_STORAGE_READ_WRITE_SELECTOR)),
        calldata: Calldata(vec![shash!(key + 1), shash!(value + 1)].into()),
        class_hash: Some(ClassHash(shash!(TEST_CLASS_HASH))),
        ..trivial_external_entry_point()
    };
    let library_entry_point = CallEntryPoint {
        entry_point_selector: EntryPointSelector(shash!(TEST_LIBRARY_CALL_SELECTOR)),
        calldata: Calldata(
            vec![
                shash!(TEST_CLASS_HASH),                  // Class hash.
                shash!(TEST_STORAGE_READ_WRITE_SELECTOR), // Storage function selector.
                shash!(2),                                // Calldata length.
                shash!(key + 1),                          // Calldata: address.
                shash!(value + 1),                        // Calldata: value.
            ]
            .into(),
        ),
        class_hash: Some(ClassHash(shash!(TEST_CLASS_HASH))),
        ..trivial_external_entry_point()
    };
    let storage_entry_point = CallEntryPoint {
        calldata: Calldata(vec![shash!(key), shash!(value)].into()),
        ..nested_storage_entry_point.clone()
    };
    let nested_storage_call_info = CallInfo {
        call: nested_storage_entry_point,
        execution: CallExecution { retdata: retdata![shash!(value + 1)] },
        ..Default::default()
    };
    let library_call_info = CallInfo {
        call: library_entry_point,
        execution: CallExecution { retdata: retdata![shash!(value + 1)] },
        inner_calls: vec![nested_storage_call_info],
        ..Default::default()
    };
    let storage_call_info = CallInfo {
        call: storage_entry_point,
        execution: CallExecution { retdata: retdata![shash!(value)] },
        ..Default::default()
    };
    let expected_call_info = CallInfo {
        call: main_entry_point.clone(),
        execution: CallExecution { retdata: retdata![shash!(0)] },
        inner_calls: vec![library_call_info, storage_call_info],
        ..Default::default()
    };

    assert_eq!(main_entry_point.execute_directly(&mut state).unwrap(), expected_call_info);
}

// TODO(Noa, 30/12/22): Add a test with no constructor
#[test]
fn test_entry_point_with_deploy_with_constructor() {
    let mut state = create_test_state();
    let calldata = Calldata(
        vec![
            shash!(TEST_CLASS_HASH), // Class hash.
            shash!(1),               // Contract_address_salt.
            shash!(2),               // Calldata length.
            shash!(1),               // Calldata: address.
            shash!(1),               // Calldata: value.
        ]
        .into(),
    );
    let entry_point_call = CallEntryPoint {
        entry_point_selector: EntryPointSelector(shash!(TEST_DEPLOY_SELECTOR)),
        calldata,
        ..trivial_external_entry_point()
    };
    assert_eq!(
        entry_point_call.execute_directly(&mut state).unwrap().execution,
        CallExecution { retdata: retdata![shash!(1)] }
    );
    let contract_address_from_state =
        *state.get_class_hash_at(ContractAddress::try_from(StarkHash::from(1)).unwrap()).unwrap();
    assert_eq!(contract_address_from_state, ClassHash(shash!(TEST_CLASS_HASH)));
}

#[test]
fn test_entry_point_with_call_contract() {
    let mut state = create_test_state();
    let calldata = Calldata(
        vec![
            shash!(TEST_CONTRACT_ADDRESS),            // Contract address.
            shash!(TEST_STORAGE_READ_WRITE_SELECTOR), // Function selector.
            shash!(2),                                // Calldata length.
            shash!(405),                              // Calldata: address.
            shash!(48),                               // Calldata: value.
        ]
        .into(),
    );
    let entry_point_call = CallEntryPoint {
        entry_point_selector: EntryPointSelector(shash!(TEST_CALL_CONTRACT_SELECTOR)),
        calldata,
        ..trivial_external_entry_point()
    };
    assert_eq!(
        entry_point_call.execute_directly(&mut state).unwrap().execution,
        CallExecution { retdata: retdata![shash!(48)] }
    );
}

// TODO(AlonH, 21/12/2022): Use storage_var with arguments after hint is implemented.
#[test]
fn test_storage_var() {
    let mut state = create_test_state();
    let entry_point_call = CallEntryPoint {
        entry_point_selector: EntryPointSelector(shash!(TEST_STORAGE_VAR_SELECTOR)),
        ..trivial_external_entry_point()
    };
    assert_eq!(
        entry_point_call.execute_directly(&mut state).unwrap().execution,
        CallExecution { retdata: retdata![] }
    );
}

#[test]
fn test_security_failure() {
    let mut state = create_security_test_state();

    fn run_security_test(
        expected_error: &str,
        entry_point_name: &str,
        calldata: Calldata,
        state: &mut CachedState<DictStateReader>,
    ) {
        let entry_point_selector = get_selector_from_name(entry_point_name);
        let entry_point_call =
            CallEntryPoint { entry_point_selector, calldata, ..trivial_external_entry_point() };
        let error = entry_point_call.execute_directly(state).unwrap_err().to_string();
        assert!(error.contains(expected_error))
    }

    for perform_inner_call_to_foo in 0..2 {
        let calldata = Calldata(vec![shash!(perform_inner_call_to_foo)].into());
        run_security_test(
            "Custom Hint Error: Out of range",
            "test_read_bad_address",
            calldata,
            &mut state,
        )
    }
}
