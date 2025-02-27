use ckb_chain_spec::consensus::ConsensusBuilder;
use ckb_crypto::secp::{Generator, Privkey};
use ckb_error::Error;
use ckb_script::{TransactionScriptsVerifier, TxVerifyEnv};
use ckb_traits::{CellDataProvider, ExtensionProvider, HeaderProvider};
use ckb_types::{
    bytes::{BufMut, Bytes, BytesMut},
    core::{
        cell::{CellMeta, CellMetaBuilder, ResolvedTransaction},
        Capacity, DepType, HeaderView, ScriptHashType, TransactionBuilder, TransactionView,
    },
    packed::{
        self, Byte32, CellDep, CellInput, CellOutput, OutPoint, Script, WitnessArgs,
        WitnessArgsBuilder,
    },
    prelude::*,
    H256,
};
use dyn_clone::{clone_trait_object, DynClone};
use hex;
use lazy_static::lazy_static;
use log::{Metadata, Record};
use rand::{distributions::Standard, thread_rng, Rng};
use secp256k1;
use serde::{Deserialize, Serialize};
use sha3::{Digest, Keccak256};
use std::{collections::HashMap, mem::size_of, result, vec};

use std::{
    process::{Child, Command},
    sync::Arc,
};
use tempdir::TempDir;

mod tests;

pub const MAX_CYCLES: u64 = std::u64::MAX;
pub const SIGNATURE_SIZE: usize = 65;
pub const RNG_SEED: u64 = 42;

lazy_static! {
    pub static ref AUTH_DEMO: Bytes = Bytes::from(&include_bytes!("../../../build/auth_demo")[..]);
    pub static ref AUTH_DL: Bytes = Bytes::from(&include_bytes!("../../../build/auth")[..]);
    pub static ref SECP256K1_DATA_BIN: Bytes =
        Bytes::from(&include_bytes!("../../../build/secp256k1_data_20210801")[..]);
    pub static ref ALWAYS_SUCCESS: Bytes =
        Bytes::from(&include_bytes!("../../../build/always_success")[..]);
}

fn _dbg_print_mem(data: &Vec<u8>, name: &str) {
    print!("rustdbg {}: (size:{})\n", name, data.len());
    let mut count = 0;
    for i in data {
        print!("0x{:02X}, ", i);
        if count % 8 == 7 {
            print!("\n");
        }
        count += 1;
    }
    print!("\n");
}

pub fn calculate_sha256(buf: &[u8]) -> [u8; 32] {
    use sha2::{Digest, Sha256};

    let mut c = Sha256::new();
    c.update(buf);
    c.finalize().into()
}

#[derive(Clone, Copy)]
pub enum AlgorithmType {
    Ckb = 0,
    Ethereum = 1,
    Eos = 2,
    Tron = 3,
    Bitcoin = 4,
    Dogecoin = 5,
    CkbMultisig = 6,
    SchnorrOrTaproot = 7,
    RSA = 8,
    Iso9796_2 = 9,
    Litecoin = 10,
    Cardano = 11,
    OwnerLock = 0xFC,
}

#[derive(Default, Clone)]
pub struct DummyDataLoader {
    pub cells: HashMap<OutPoint, (CellOutput, ckb_types::bytes::Bytes)>,
}

impl DummyDataLoader {
    pub fn new() -> Self {
        Self::default()
    }
}

impl CellDataProvider for DummyDataLoader {
    // load Cell Data
    fn load_cell_data(&self, cell: &CellMeta) -> Option<ckb_types::bytes::Bytes> {
        cell.mem_cell_data.clone().or_else(|| {
            self.cells
                .get(&cell.out_point)
                .map(|(_, data)| data.clone())
        })
    }

    fn load_cell_data_hash(&self, cell: &CellMeta) -> Option<Byte32> {
        self.load_cell_data(cell)
            .map(|e| CellOutput::calc_data_hash(&e))
    }

    fn get_cell_data(&self, _out_point: &OutPoint) -> Option<ckb_types::bytes::Bytes> {
        None
    }

    fn get_cell_data_hash(&self, _out_point: &OutPoint) -> Option<Byte32> {
        None
    }
}

impl HeaderProvider for DummyDataLoader {
    fn get_header(&self, _hash: &Byte32) -> Option<HeaderView> {
        None
    }
}

impl ExtensionProvider for DummyDataLoader {
    fn get_block_extension(&self, _hash: &packed::Byte32) -> Option<packed::Bytes> {
        None
    }
}

pub fn sign_tx(tx: TransactionView, config: &TestConfig) -> TransactionView {
    let witnesses_len = tx.witnesses().len();
    sign_tx_by_input_group(tx, config, 0, witnesses_len)
}

pub fn get_message_to_sign(tx: TransactionView, config: &TestConfig) -> H256 {
    let witnesses_len = tx.witnesses().len();
    get_message_to_sign_by_input_group(tx, config, 0, witnesses_len)
}

pub fn get_message_to_sign_by_input_group(
    tx: TransactionView,
    config: &TestConfig,
    begin_index: usize,
    len: usize,
) -> H256 {
    let tx_hash = tx.hash();
    tx.inputs()
        .into_iter()
        .enumerate()
        .find_map(|(i, _)| {
            if i == begin_index {
                let mut blake2b = ckb_hash::new_blake2b();
                let mut message = [0u8; 32];
                blake2b.update(&tx_hash.raw_data());
                // digest the first witness
                let witness = WitnessArgs::new_unchecked(tx.witnesses().get(i).unwrap().unpack());
                let zero_lock: Bytes = {
                    let mut buf = Vec::new();
                    buf.resize(config.auth.get_sign_size(), 0);
                    buf.into()
                };
                let witness_for_digest = witness
                    .clone()
                    .as_builder()
                    .lock(Some(zero_lock).pack())
                    .build();
                let witness_len = witness_for_digest.as_bytes().len() as u64;
                blake2b.update(&witness_len.to_le_bytes());
                blake2b.update(&witness_for_digest.as_bytes());
                ((i + 1)..(i + len)).for_each(|n| {
                    let witness = tx.witnesses().get(n).unwrap();
                    let witness_len = witness.raw_data().len() as u64;
                    blake2b.update(&witness_len.to_le_bytes());
                    blake2b.update(&witness.raw_data());
                });
                blake2b.finalize(&mut message);
                Some(config.auth.convert_message(&message))
            } else {
                None
            }
        })
        .unwrap()
}

pub fn set_signature(tx: TransactionView, signature: &Bytes) -> TransactionView {
    set_signature_by_index(tx, signature, 0)
}

pub fn set_signature_by_index(
    tx: TransactionView,
    signature: &Bytes,
    begin_index: usize,
) -> TransactionView {
    // We need to pass to ownership of signature to the closure in map below.
    let mut signed_witnesses: Vec<packed::Bytes> = tx
        .inputs()
        .into_iter()
        .enumerate()
        .map(|(i, _)| {
            if i == begin_index {
                let witness =
                    WitnessArgs::new_unchecked(tx.witnesses().get(i).unwrap_or_default().unpack());
                witness
                    .as_builder()
                    .lock(Some(signature.clone()).pack())
                    .build()
                    .as_bytes()
                    .pack()
            } else {
                tx.witnesses().get(i).unwrap_or_default()
            }
        })
        .collect();
    for i in signed_witnesses.len()..tx.witnesses().len() {
        signed_witnesses.push(tx.witnesses().get(i).unwrap());
    }
    // calculate message
    tx.as_advanced_builder()
        .set_witnesses(signed_witnesses)
        .build()
}

pub fn sign_tx_by_input_group(
    tx: TransactionView,
    config: &TestConfig,
    begin_index: usize,
    len: usize,
) -> TransactionView {
    let mut rng = thread_rng();
    let tx_hash = tx.hash();
    let mut signed_witnesses: Vec<packed::Bytes> = tx
        .inputs()
        .into_iter()
        .enumerate()
        .map(|(i, _)| {
            if i == begin_index {
                let mut blake2b = ckb_hash::new_blake2b();
                let mut message = [0u8; 32];
                blake2b.update(&tx_hash.raw_data());
                // digest the first witness
                let witness = WitnessArgs::new_unchecked(tx.witnesses().get(i).unwrap().unpack());
                let zero_lock: Bytes = {
                    let mut buf = Vec::new();
                    buf.resize(config.auth.get_sign_size(), 0);
                    buf.into()
                };
                let witness_for_digest = witness
                    .clone()
                    .as_builder()
                    .lock(Some(zero_lock).pack())
                    .build();
                let witness_len = witness_for_digest.as_bytes().len() as u64;
                blake2b.update(&witness_len.to_le_bytes());
                blake2b.update(&witness_for_digest.as_bytes());
                ((i + 1)..(i + len)).for_each(|n| {
                    let witness = tx.witnesses().get(n).unwrap();
                    let witness_len = witness.raw_data().len() as u64;
                    blake2b.update(&witness_len.to_le_bytes());
                    blake2b.update(&witness.raw_data());
                });
                blake2b.finalize(&mut message);
                if config.incorrect_msg {
                    rng.fill(&mut message);
                }
                let sig;
                if config.incorrect_sign {
                    sig = {
                        let buff: Vec<u8> = rng.sample_iter(&Standard).take(16).collect();
                        Bytes::from(buff)
                    };
                } else {
                    sig = config.auth.sign(&config.auth.convert_message(&message));
                }

                let sig2 = match config.incorrect_sign_size {
                    TestConfigIncorrectSing::None => sig,
                    TestConfigIncorrectSing::Bigger => {
                        let sign_size = rng.gen_range(1, 64);
                        let mut buff = BytesMut::with_capacity(sig.len() + sign_size);
                        buff.put(sig);
                        let mut fillbuffer: BytesMut = BytesMut::with_capacity(sign_size);
                        for _i in 0..(sign_size - 1) {
                            fillbuffer.put_u8(rng.gen_range(0, 255) as u8);
                        }
                        buff.put(Bytes::from(fillbuffer));
                        buff.freeze()
                    }
                    TestConfigIncorrectSing::Smaller => {
                        let sign_size = rng.gen_range(1, sig.len() - 8);
                        let temp_sig = &sig.to_vec()[0..sign_size];
                        Bytes::from(temp_sig.to_vec())
                    }
                };

                witness
                    .as_builder()
                    .lock(Some(sig2).pack())
                    .build()
                    .as_bytes()
                    .pack()
            } else {
                tx.witnesses().get(i).unwrap_or_default()
            }
        })
        .collect();
    for i in signed_witnesses.len()..tx.witnesses().len() {
        signed_witnesses.push(tx.witnesses().get(i).unwrap());
    }
    // calculate message
    tx.as_advanced_builder()
        .set_witnesses(signed_witnesses)
        .build()
}

fn append_cell_deps<R: Rng>(
    dummy: &mut DummyDataLoader,
    rng: &mut R,
    deps_data: &Bytes,
) -> OutPoint {
    // setup sighash_all dep
    let sighash_all_out_point = {
        let contract_tx_hash = {
            let mut buf = [0u8; 32];
            rng.fill(&mut buf);
            buf.pack()
        };
        OutPoint::new(contract_tx_hash, 0)
    };

    // dep contract code
    let sighash_all_cell = CellOutput::new_builder()
        .capacity(
            Capacity::bytes(deps_data.len())
                .expect("script capacity")
                .pack(),
        )
        .build();
    dummy.cells.insert(
        sighash_all_out_point.clone(),
        (sighash_all_cell, deps_data.clone()),
    );

    sighash_all_out_point
}

fn append_cells_deps<R: Rng>(
    dummy: &mut DummyDataLoader,
    rng: &mut R,
) -> (Capacity, TransactionBuilder) {
    let sighash_all_out_point = append_cell_deps(dummy, rng, &AUTH_DEMO);
    let sighash_dl_out_point = append_cell_deps(dummy, rng, &AUTH_DL);
    let always_success_out_point = append_cell_deps(dummy, rng, &ALWAYS_SUCCESS);
    let secp256k1_data_out_point = append_cell_deps(dummy, rng, &SECP256K1_DATA_BIN);

    // setup default tx builder
    let dummy_capacity = Capacity::shannons(42);
    let tx_builder = TransactionBuilder::default()
        .cell_dep(
            CellDep::new_builder()
                .out_point(sighash_all_out_point)
                .dep_type(DepType::Code.into())
                .build(),
        )
        .cell_dep(
            CellDep::new_builder()
                .out_point(sighash_dl_out_point)
                .dep_type(DepType::Code.into())
                .build(),
        )
        .cell_dep(
            CellDep::new_builder()
                .out_point(always_success_out_point)
                .dep_type(DepType::Code.into())
                .build(),
        )
        .cell_dep(
            CellDep::new_builder()
                .out_point(secp256k1_data_out_point)
                .dep_type(DepType::Code.into())
                .build(),
        )
        .output(
            CellOutput::new_builder()
                .capacity(dummy_capacity.pack())
                .build(),
        )
        .output_data(Bytes::new().pack());
    (dummy_capacity, tx_builder)
}

pub fn gen_tx_with_pub_key_hash(
    dummy: &mut DummyDataLoader,
    config: &TestConfig,
    hash: Vec<u8>,
) -> TransactionView {
    let lock_args = gen_args_with_pub_key_hash(&config, hash);
    // Note that we use deterministic here to ensure the same transaction structure
    // is generated.
    let mut rng: rand::rngs::SmallRng = rand::SeedableRng::seed_from_u64(RNG_SEED);

    gen_tx_with_grouped_args(
        dummy,
        vec![(lock_args, config.sign_size as usize)],
        &mut rng,
    )
}

pub fn gen_tx(dummy: &mut DummyDataLoader, config: &TestConfig) -> TransactionView {
    let lock_args = gen_args(&config);

    let mut rng = thread_rng();
    gen_tx_with_grouped_args(
        dummy,
        vec![(lock_args, config.sign_size as usize)],
        &mut rng,
    )
}

pub fn gen_tx_with_grouped_args<R: Rng>(
    dummy: &mut DummyDataLoader,
    grouped_args: Vec<(Bytes, usize)>,
    rng: &mut R,
) -> TransactionView {
    let (dummy_capacity, mut tx_builder) = append_cells_deps(dummy, rng);
    let sighash_all_cell_data_hash = CellOutput::calc_data_hash(&AUTH_DEMO);

    for (args, inputs_size) in grouped_args {
        // setup dummy input unlock script
        for _ in 0..inputs_size {
            let previous_tx_hash = {
                let mut buf = [0u8; 32];
                rng.fill(&mut buf);
                buf.pack()
            };
            let previous_out_point = OutPoint::new(previous_tx_hash, 0);
            let script = Script::new_builder()
                .args(args.pack())
                .code_hash(sighash_all_cell_data_hash.clone())
                .hash_type(ScriptHashType::Data2.into())
                .build();
            let previous_output_cell = CellOutput::new_builder()
                .capacity(dummy_capacity.pack())
                .lock(script)
                .build();
            dummy.cells.insert(
                previous_out_point.clone(),
                (previous_output_cell.clone(), Bytes::new()),
            );
            let mut random_extra_witness = [0u8; 64];
            rng.fill(&mut random_extra_witness);

            let witness_args = WitnessArgsBuilder::default()
                .input_type(Some(Bytes::from(random_extra_witness.to_vec())).pack())
                .build();
            tx_builder = tx_builder
                .input(CellInput::new(previous_out_point, 0))
                .witness(witness_args.as_bytes().pack());
        }
    }

    tx_builder.build()
}

#[derive(Serialize, Deserialize)]
struct CkbAuthType {
    algorithm_id: u8,
    content: [u8; 20],
}

#[derive(Serialize, Deserialize)]
struct EntryType {
    code_hash: [u8; 32],
    hash_type: u8,
    entry_category: u8,
}

#[derive(Clone, Copy)]
pub enum EntryCategoryType {
    // Exec = 0,
    DynamicLinking = 1,
    Spawn = 2,
}

#[derive(PartialEq, Eq)]
pub enum TestConfigIncorrectSing {
    None,
    Bigger,
    Smaller,
}

pub struct TestConfig {
    pub auth: Box<dyn Auth>,
    pub entry_category_type: EntryCategoryType,

    pub sign_size: i32,

    pub incorrect_pubkey: bool,
    pub incorrect_msg: bool,
    pub incorrect_sign: bool,
    pub incorrect_sign_size: TestConfigIncorrectSing,
}

impl TestConfig {
    pub fn new(
        auth: &Box<dyn Auth>,
        entry_category_type: EntryCategoryType,
        sign_size: i32,
    ) -> TestConfig {
        assert!(sign_size > 0);
        TestConfig {
            auth: auth.clone(),
            entry_category_type,
            sign_size,
            incorrect_pubkey: false,
            incorrect_msg: false,
            incorrect_sign: false,
            incorrect_sign_size: TestConfigIncorrectSing::None,
        }
    }
}

pub fn gen_args(config: &TestConfig) -> Bytes {
    do_gen_args(config, None)
}

pub fn gen_args_with_pub_key_hash(config: &TestConfig, pub_key_hash: Vec<u8>) -> Bytes {
    do_gen_args(config, Some(pub_key_hash))
}

pub fn do_gen_args(config: &TestConfig, pub_key_hash: Option<Vec<u8>>) -> Bytes {
    let mut ckb_auth_type = CkbAuthType {
        algorithm_id: config.auth.get_algorithm_type(),
        content: [0; 20],
    };

    let mut entry_type = EntryType {
        code_hash: [0; 32],
        hash_type: ScriptHashType::Data2.into(),
        entry_category: config.entry_category_type.clone() as u8,
    };

    if !config.incorrect_pubkey {
        let pub_hash = pub_key_hash.unwrap_or(config.auth.get_pub_key_hash());
        assert_eq!(pub_hash.len(), 20);
        ckb_auth_type.content.copy_from_slice(pub_hash.as_slice());
    } else {
        let mut rng = thread_rng();
        let incorrect_pubkey = {
            let mut buf = [0u8; 32];
            rng.fill(&mut buf);
            Vec::from(buf)
        };
        ckb_auth_type
            .content
            .copy_from_slice(&incorrect_pubkey.as_slice()[0..20]);
    }

    let sighash_all_cell_data_hash = CellOutput::calc_data_hash(&AUTH_DL);
    entry_type
        .code_hash
        .copy_from_slice(sighash_all_cell_data_hash.as_slice());

    let mut bytes = BytesMut::with_capacity(size_of::<CkbAuthType>() + size_of::<EntryType>());
    bytes.put(Bytes::from(bincode::serialize(&ckb_auth_type).unwrap()));
    bytes.put(Bytes::from(bincode::serialize(&entry_type).unwrap()));

    bytes.freeze()
}

pub fn build_resolved_tx(
    data_loader: &DummyDataLoader,
    tx: &TransactionView,
) -> ResolvedTransaction {
    let resolved_cell_deps = tx
        .cell_deps()
        .into_iter()
        .map(|deps_out_point| {
            let (dep_output, dep_data) =
                data_loader.cells.get(&deps_out_point.out_point()).unwrap();
            CellMetaBuilder::from_cell_output(dep_output.to_owned(), dep_data.to_owned())
                .out_point(deps_out_point.out_point())
                .build()
        })
        .collect();

    let mut resolved_inputs = Vec::new();
    for i in 0..tx.inputs().len() {
        let previous_out_point = tx.inputs().get(i).unwrap().previous_output();
        let (input_output, input_data) = data_loader.cells.get(&previous_out_point).unwrap();
        resolved_inputs.push(
            CellMetaBuilder::from_cell_output(input_output.to_owned(), input_data.to_owned())
                .out_point(previous_out_point)
                .build(),
        );
    }

    ResolvedTransaction {
        transaction: tx.clone(),
        resolved_cell_deps,
        resolved_inputs,
        resolved_dep_groups: vec![],
    }
}

pub fn debug_printer(_script: &Byte32, msg: &str) {
    /*
    let slice = _script.as_slice();
    let str = format!(
        "Script({:x}{:x}{:x}{:x}{:x})",
        slice[0], slice[1], slice[2], slice[3], slice[4]
    );
    println!("{:?}: {}", str, msg);
    */
    println!("{}", msg);
}

pub struct MyLogger;

impl log::Log for MyLogger {
    fn enabled(&self, _metadata: &Metadata) -> bool {
        true
    }

    fn log(&self, record: &Record) {
        println!("{}:{} - {}", record.level(), record.target(), record.args());
    }
    fn flush(&self) {}
}

pub enum AuthErrorCodeType {
    NotImplemented = 100,
    Mismatched,
    InvalidArg,
    ErrorWrongState,
    // exec
    ExecInvalidLength,
    ExecInvalidParam,
    ExecNotPaired,
    ExecInvalidSig,
    ExecInvalidMsg,
}

pub fn assert_script_error(err: Error, err_code: AuthErrorCodeType, des: &str) {
    let err_code = err_code as i8;
    let error_string = err.to_string();
    assert!(
        error_string.contains(format!("error code {}", err_code).as_str()),
        "{}, error string: {}, expected error code: {}",
        des,
        error_string,
        err_code
    );
}

pub fn assert_script_error_vec(err: Error, err_codes: &[i32]) {
    let error_string = err.to_string();
    let mut is_assert = false;
    for err_code in err_codes {
        if error_string.contains(format!("error code {}", err_code).as_str()) {
            is_assert = true;
            break;
        }
    }

    if !is_assert {
        assert!(false, "error_string: {}", error_string);
    }
}

pub fn assert_script_error_i(err: Error, err_code: i32) {
    let err_code = err_code as i8;
    let error_string = err.to_string();
    assert!(
        error_string.contains(format!("error code {}", err_code).as_str()),
        "error_string: {}, expected_error_code: {}",
        error_string,
        err_code
    );
}

pub trait Auth: DynClone {
    fn get_pub_key_hash(&self) -> Vec<u8>; // result size must is 20
    fn get_algorithm_type(&self) -> u8;

    fn convert_message(&self, message: &[u8; 32]) -> H256 {
        H256::from(message.clone())
    }
    fn sign(&self, msg: &H256) -> Bytes;
    fn message(&self) -> Bytes {
        Bytes::new()
    }
    fn get_sign_size(&self) -> usize {
        SIGNATURE_SIZE
    }
}

pub fn auth_builder(t: AlgorithmType, official: bool) -> result::Result<Box<dyn Auth>, i32> {
    match t {
        AlgorithmType::Ckb => {
            return Ok(CKbAuth::new());
        }
        AlgorithmType::Ethereum => {
            return Ok(EthereumAuth::new());
        }
        AlgorithmType::Eos => {
            return Ok(EosAuth::new());
        }
        AlgorithmType::Tron => {
            return Ok(TronAuth::new());
        }
        AlgorithmType::Bitcoin => {
            return Ok(BitcoinAuth::new());
        }
        AlgorithmType::Dogecoin => {
            return Ok(DogecoinAuth::new());
        }
        AlgorithmType::CkbMultisig => {}
        AlgorithmType::SchnorrOrTaproot => {
            return Ok(SchnorrAuth::new());
        }
        AlgorithmType::RSA => {
            return Ok(RSAAuth::new());
        }
        AlgorithmType::Iso9796_2 => {}
        AlgorithmType::Litecoin => {
            return Ok(LitecoinAuth::new_official(official));
        }
        AlgorithmType::Cardano => {
            panic!("unsupport cardano")
        }
        AlgorithmType::OwnerLock => {
            return Ok(OwnerLockAuth::new());
        }
    }
    assert!(false);
    Err(1)
}
clone_trait_object!(Auth);

#[derive(Clone)]
pub struct CKbAuth {
    pub privkey: Privkey,
}
impl CKbAuth {
    fn generator_key() -> Privkey {
        Generator::random_privkey()
    }
    fn new() -> Box<dyn Auth> {
        Box::new(CKbAuth {
            privkey: CKbAuth::generator_key(),
        })
    }
    fn get_ckb_pub_key_hash(privkey: &Privkey) -> Vec<u8> {
        let pub_key = privkey.pubkey().expect("pubkey").serialize();
        let pub_hash = ckb_hash::blake2b_256(pub_key.as_slice());
        Vec::from(&pub_hash[0..20])
    }
    pub fn ckb_sign(msg: &H256, privkey: &Privkey) -> Bytes {
        let sig = privkey.sign_recoverable(&msg).expect("sign").serialize();
        Bytes::from(sig)
    }
}
impl Auth for CKbAuth {
    fn get_pub_key_hash(&self) -> Vec<u8> {
        CKbAuth::get_ckb_pub_key_hash(&self.privkey)
    }
    fn get_algorithm_type(&self) -> u8 {
        AlgorithmType::Ckb as u8
    }
    fn sign(&self, msg: &H256) -> Bytes {
        CKbAuth::ckb_sign(msg, &self.privkey)
    }
}

#[derive(Clone)]
pub struct EthereumAuth {
    pub privkey: secp256k1::SecretKey,
    pub pubkey: secp256k1::PublicKey,
}
impl EthereumAuth {
    fn new() -> Box<dyn Auth> {
        let generator: secp256k1::Secp256k1<secp256k1::All> = secp256k1::Secp256k1::new();
        let mut rng = thread_rng();
        let (privkey, pubkey) = generator.generate_keypair(&mut rng);
        Box::new(EthereumAuth { privkey, pubkey })
    }
    pub fn get_eth_pub_key_hash(pubkey: &secp256k1::PublicKey) -> Vec<u8> {
        let pubkey = pubkey.serialize_uncompressed();
        let mut hasher = Keccak256::new();
        hasher.update(&pubkey[1..].to_vec());
        let r = hasher.finalize().as_slice().to_vec();

        Vec::from(&r[12..])
    }
    pub fn eth_sign(msg: &H256, privkey: &secp256k1::SecretKey) -> Bytes {
        let secp: secp256k1::Secp256k1<secp256k1::All> = secp256k1::Secp256k1::gen_new();
        let msg = secp256k1::Message::from_slice(msg.as_bytes()).unwrap();
        let sign = secp.sign_ecdsa_recoverable(&msg, privkey);
        let (rid, sign) = sign.serialize_compact();

        let mut data = [0; 65];
        data[0..64].copy_from_slice(&sign[0..64]);
        data[64] = rid.to_i32() as u8;
        let sign = ckb_crypto::secp::Signature::from_slice(&data).unwrap();
        Bytes::from(sign.serialize())
    }
}
impl Auth for EthereumAuth {
    fn get_pub_key_hash(&self) -> Vec<u8> {
        EthereumAuth::get_eth_pub_key_hash(&self.pubkey)
    }
    fn get_algorithm_type(&self) -> u8 {
        AlgorithmType::Ethereum as u8
    }
    fn convert_message(&self, message: &[u8; 32]) -> H256 {
        let eth_prefix: &[u8; 28] = b"\x19Ethereum Signed Message:\n32";
        let mut hasher = Keccak256::new();
        hasher.update(eth_prefix);
        hasher.update(message);
        let r = hasher.finalize();
        let ret = H256::from_slice(r.as_slice()).expect("convert_keccak256_hash");
        ret
    }
    fn sign(&self, msg: &H256) -> Bytes {
        Self::eth_sign(msg, &self.privkey)
    }
}

#[derive(Clone)]
pub struct EosAuth {
    pub privkey: secp256k1::SecretKey,
    pub pubkey: secp256k1::PublicKey,
}
impl EosAuth {
    fn new() -> Box<dyn Auth> {
        let generator: secp256k1::Secp256k1<secp256k1::All> = secp256k1::Secp256k1::new();
        let mut rng = thread_rng();
        let (privkey, pubkey) = generator.generate_keypair(&mut rng);
        Box::new(EosAuth { privkey, pubkey })
    }
}
impl Auth for EosAuth {
    fn get_pub_key_hash(&self) -> Vec<u8> {
        EthereumAuth::get_eth_pub_key_hash(&self.pubkey)
    }
    fn get_algorithm_type(&self) -> u8 {
        AlgorithmType::Eos as u8
    }
    fn convert_message(&self, message: &[u8; 32]) -> H256 {
        let msg = calculate_sha256(message);
        H256::from(msg)
    }
    fn sign(&self, msg: &H256) -> Bytes {
        EthereumAuth::eth_sign(msg, &self.privkey)
    }
}

#[derive(Clone)]
pub struct TronAuth {
    pub privkey: secp256k1::SecretKey,
    pub pubkey: secp256k1::PublicKey,
}
impl TronAuth {
    fn new() -> Box<dyn Auth> {
        let generator: secp256k1::Secp256k1<secp256k1::All> = secp256k1::Secp256k1::new();
        let mut rng = thread_rng();
        let (privkey, pubkey) = generator.generate_keypair(&mut rng);
        Box::new(TronAuth { privkey, pubkey })
    }
}
impl Auth for TronAuth {
    fn get_pub_key_hash(&self) -> Vec<u8> {
        EthereumAuth::get_eth_pub_key_hash(&self.pubkey)
    }
    fn get_algorithm_type(&self) -> u8 {
        AlgorithmType::Tron as u8
    }
    fn convert_message(&self, message: &[u8; 32]) -> H256 {
        let eth_prefix: &[u8; 24] = b"\x19TRON Signed Message:\n32";
        let mut hasher = Keccak256::new();
        hasher.update(eth_prefix);
        hasher.update(message);
        let r = hasher.finalize();
        H256::from_slice(r.as_slice()).expect("convert_keccak256_hash")
    }
    fn sign(&self, msg: &H256) -> Bytes {
        EthereumAuth::eth_sign(msg, &self.privkey)
    }
}

#[derive(Clone)]
pub struct BitcoinAuth {
    pub privkey: Privkey,
    pub compress: bool,
}
impl BitcoinAuth {
    pub fn new() -> Box<BitcoinAuth> {
        let privkey = Generator::random_privkey();
        Box::new(BitcoinAuth {
            privkey,
            compress: true,
        })
    }
    pub fn get_btc_pub_key_hash(privkey: &Privkey, compress: bool) -> Vec<u8> {
        use mbedtls::hash::{Md, Type};

        let pub_key = privkey.pubkey().expect("pubkey");
        let pub_key_vec: Vec<u8>;
        if compress {
            pub_key_vec = pub_key.serialize();
        } else {
            let mut temp: BytesMut = BytesMut::with_capacity(65);
            temp.put_u8(4);
            temp.put(Bytes::from(pub_key.as_bytes().to_vec()));
            pub_key_vec = temp.freeze().to_vec();
        }

        let pub_hash = calculate_sha256(&pub_key_vec);

        let mut msg = [0u8; 20];
        Md::hash(Type::Ripemd, &pub_hash, &mut msg).expect("hash ripemd");
        msg.to_vec()
    }
    pub fn btc_convert_message(message: &[u8; 32]) -> H256 {
        let message_magic = b"\x18Bitcoin Signed Message:\n\x40";
        let msg_hex = hex::encode(message);
        assert_eq!(msg_hex.len(), 64);

        let mut temp2: BytesMut = BytesMut::with_capacity(message_magic.len() + msg_hex.len());
        temp2.put(Bytes::from(message_magic.to_vec()));
        temp2.put(Bytes::from(hex::encode(message)));

        let msg = calculate_sha256(&temp2);
        let msg = calculate_sha256(&msg);

        H256::from(msg)
    }
    pub fn btc_sign(msg: &H256, privkey: &Privkey, compress: bool) -> Bytes {
        let sign = privkey.sign_recoverable(&msg).expect("sign").serialize();
        assert_eq!(sign.len(), 65);
        let recid = sign[64];

        let mark: u8;
        if compress {
            mark = recid + 31;
        } else {
            mark = recid + 27;
        };
        let mut ret = BytesMut::with_capacity(65);
        ret.put_u8(mark);
        ret.put(&sign[0..64]);
        Bytes::from(ret)
    }
}
impl Auth for BitcoinAuth {
    fn get_pub_key_hash(&self) -> Vec<u8> {
        BitcoinAuth::get_btc_pub_key_hash(&self.privkey, self.compress)
    }
    fn get_algorithm_type(&self) -> u8 {
        AlgorithmType::Bitcoin as u8
    }
    fn convert_message(&self, message: &[u8; 32]) -> H256 {
        BitcoinAuth::btc_convert_message(message)
    }
    fn sign(&self, msg: &H256) -> Bytes {
        BitcoinAuth::btc_sign(msg, &self.privkey, self.compress)
    }
}

#[derive(Clone)]
pub struct DogecoinAuth {
    pub privkey: Privkey,
    pub compress: bool,
}
impl DogecoinAuth {
    pub fn new() -> Box<DogecoinAuth> {
        let privkey = Generator::random_privkey();
        Box::new(DogecoinAuth {
            privkey,
            compress: true,
        })
    }
}
impl Auth for DogecoinAuth {
    fn get_pub_key_hash(&self) -> Vec<u8> {
        BitcoinAuth::get_btc_pub_key_hash(&self.privkey, self.compress)
    }
    fn get_algorithm_type(&self) -> u8 {
        AlgorithmType::Dogecoin as u8
    }
    fn convert_message(&self, message: &[u8; 32]) -> H256 {
        let message_magic = b"\x19Dogecoin Signed Message:\n\x40";
        let msg_hex = hex::encode(message);
        assert_eq!(msg_hex.len(), 64);

        let mut temp2: BytesMut = BytesMut::with_capacity(message_magic.len() + msg_hex.len());
        temp2.put(Bytes::from(message_magic.to_vec()));
        temp2.put(Bytes::from(hex::encode(message)));

        let msg = calculate_sha256(&temp2);
        let msg = calculate_sha256(&msg);

        H256::from(msg)
    }
    fn sign(&self, msg: &H256) -> Bytes {
        BitcoinAuth::btc_sign(msg, &self.privkey, self.compress)
    }
}

#[derive(Clone)]
pub struct LitecoinAuth {
    // whether to use official tools to sign messages
    pub official: bool,
    // Use raw [u8; 32] to easily convert this into Privkey and SecretKey
    pub sk: [u8; 32],
    pub compress: bool,
    pub network: bitcoin::Network,
}
impl LitecoinAuth {
    pub fn new() -> Box<LitecoinAuth> {
        let sk: [u8; 32] = Generator::random_secret_key().secret_bytes();
        Box::new(LitecoinAuth {
            official: false,
            sk,
            compress: true,
            network: bitcoin::Network::Testnet,
        })
    }
    pub fn new_official(official: bool) -> Box<LitecoinAuth> {
        let mut auth = Self::new();
        auth.official = official;
        auth
    }
    pub fn get_privkey(&self) -> Privkey {
        Privkey::from_slice(&self.sk)
    }
    pub fn get_btc_private_key(&self) -> bitcoin::PrivateKey {
        let sk = bitcoin::secp256k1::SecretKey::from_slice(&self.sk).unwrap();
        bitcoin::PrivateKey::new(sk, self.network)
    }
}
impl Auth for LitecoinAuth {
    fn get_pub_key_hash(&self) -> Vec<u8> {
        let hash = BitcoinAuth::get_btc_pub_key_hash(&self.get_privkey(), self.compress);
        hash
    }
    fn get_algorithm_type(&self) -> u8 {
        AlgorithmType::Litecoin as u8
    }
    fn convert_message(&self, message: &[u8; 32]) -> H256 {
        if self.official {
            return H256::from(message.clone());
        }
        let message_magic = b"\x19Litecoin Signed Message:\n\x40";
        let msg_hex = hex::encode(message);
        assert_eq!(msg_hex.len(), 64);

        let mut temp2: BytesMut = BytesMut::with_capacity(message_magic.len() + msg_hex.len());
        temp2.put(Bytes::from(message_magic.to_vec()));
        temp2.put(Bytes::from(hex::encode(message)));

        let msg = calculate_sha256(&temp2);
        let msg = calculate_sha256(&msg);

        H256::from(msg)
    }
    fn sign(&self, msg: &H256) -> Bytes {
        if !self.official {
            return BitcoinAuth::btc_sign(msg, &self.get_privkey(), self.compress);
        }
        let daemon = LitecoinDaemon::new();
        let wallet_name = "ckb-auth-test-wallet";
        let rpc_wallet_argument = format!("-rpcwallet={}", wallet_name);
        let rpc_wallet_argument = rpc_wallet_argument.as_str();
        let test_private_key_label = "ckb-auth-test-privkey";
        let privkey = self.get_btc_private_key();
        let privkey_wif = privkey.to_wif();
        let message = hex::encode(msg);
        // Create a wallet
        assert!(
            daemon
                .get_client_command()
                .args(vec!["createwallet", wallet_name])
                .status()
                .unwrap()
                .success(),
            "creating wallet failed"
        );

        // Import the private key
        assert!(
            daemon
                .get_client_command()
                .args(vec![
                    rpc_wallet_argument,
                    "importprivkey",
                    &privkey_wif,
                    test_private_key_label,
                    "false"
                ])
                .status()
                .unwrap()
                .success(),
            "importing private key failed"
        );

        // Dump the wallet to get address. We found no easier way to get address that work with
        // signmessage and verifymessage.
        let wallet_dump = daemon.data_dir.path().join("ckb-auth-test-wallet-dump");
        let wallet_dump = wallet_dump.to_str().expect("valid file path");
        assert!(
            daemon
                .get_client_command()
                .args(vec![rpc_wallet_argument, "dumpwallet", wallet_dump])
                .status()
                .unwrap()
                .success(),
            "dumping wallet failed"
        );

        // Example dump file line
        // cQoJiU5ECnVpRqfV5dWKDE2sLQq6516Tja1Hb1GABUV24n7WkqV4 1970-01-01T00:00:01Z label=ckb-auth-test-privkey # addr=mhknqLHQGWDXuLsPdzab8nA4jD3fMdVYS2,QjpdvL4h5jnfaj1uV5ifJNUAYZTTbjgFH5,tltc1qrz8z67vtu38pq2yzqtq7unftmsaueq6a8da5n2,tmweb1qqvx9sdnuzgv0jq3mlhcq4ttwx8haw8wgskegd0w298hqqqpf300msqemjfm7c2v7gt5sl5snf9kr6tygl3t773l6spt4cmuel4d92m038g8qtmlm
        let mut pubkey = None;
        let file_content = std::fs::read_to_string(wallet_dump).expect("valid wallet dump file");
        for line in file_content.lines() {
            if line.starts_with(&privkey_wif) {
                for field in line.split_whitespace() {
                    let prefix = "addr=";
                    if field.starts_with(prefix) {
                        let mut addresses = field[prefix.len()..].split(",");
                        pubkey = addresses.next();
                        break;
                    }
                }
            }
        }
        let pubkey = pubkey.expect("correctly imported private key");

        // Sign the message
        let output = daemon
            .get_client_command()
            .args(vec![rpc_wallet_argument, "signmessage", pubkey, &message])
            .output()
            .unwrap();
        if !output.status.success() {
            panic!(
                "signing message failed: status {}, stdout {} stderr {:?}",
                output.status,
                std::str::from_utf8(&output.stdout).unwrap_or(&format!("{:?}", &output.stdout)),
                std::str::from_utf8(&output.stderr).unwrap_or(&format!("{:?}", &output.stderr)),
            );
        }
        let signature_base64 = std::str::from_utf8(&output.stdout).unwrap().trim();
        use base64::{engine::general_purpose, Engine as _};
        let signature = general_purpose::STANDARD
            .decode(signature_base64)
            .expect("valid output");

        // Verify this signature anyway to make sure nothing is wrong.
        let verification_output = daemon
            .get_client_command()
            .args(vec![
                rpc_wallet_argument,
                "verifymessage",
                pubkey,
                signature_base64,
                &message,
            ])
            .output()
            .unwrap();
        assert!(verification_output.status.success(), "verification failed");
        let verification_stdout = std::str::from_utf8(&verification_output.stdout)
            .unwrap()
            .trim();
        assert_eq!(verification_stdout, "true", "verification failed");

        signature.into()
    }
}

pub struct ProcessGuard(Child);

impl Drop for ProcessGuard {
    fn drop(&mut self) {
        // You can check std::thread::panicking() here
        match self.0.kill() {
            Err(e) => println!("Could not kill child process: {}", e),
            Ok(_) => println!("Successfully killed child process"),
        }
    }
}

pub struct LitecoinDaemon {
    data_dir: tempdir::TempDir,
    #[allow(dead_code)]
    process_guard: ProcessGuard,
    client_executable: String,
    common_arguments: Vec<String>,
}

impl LitecoinDaemon {
    fn new() -> Self {
        let executable = "litecoind";
        let client_executable = "litecoin-cli".to_string();

        let data_dir = TempDir::new(executable).expect("get temp directory");
        let temp_dir = data_dir.path().to_str().expect("path as str");
        let common_arguments = vec!["-testnet".to_string(), format!("-datadir={}", temp_dir)];
        // TODO: maybe listen to a random port.
        let process_guard = ProcessGuard(
            Command::new(executable)
                .args(&common_arguments)
                .arg("-whitelist=1.1.1.1/32")
                .spawn()
                .expect("spawn subprocess"),
        );

        let daemon = Self {
            data_dir,
            process_guard,
            client_executable,
            common_arguments,
        };

        let num_of_retries = 10;
        for i in 1..=num_of_retries {
            let mut command = daemon.get_client_command();
            if command.arg("ping").status().expect("run client").success() {
                break;
            }
            if i == num_of_retries {
                panic!("Unable to connect to the daemon");
            }

            std::thread::sleep(std::time::Duration::from_secs(1));
        }

        daemon
    }

    fn get_client_command(&self) -> Command {
        let mut command = Command::new(&self.client_executable);
        command.args(&self.common_arguments);
        command
    }
}

#[derive(Clone)]
pub struct CkbMultisigAuth {
    pub pubkeys_cnt: u8,
    pub threshold: u8,

    pub pubkey_data: Vec<u8>,
    pub privkeys: Vec<Privkey>,
    pub hash: Vec<u8>,
}
impl CkbMultisigAuth {
    pub fn get_mulktisig_size(&self) -> usize {
        (4 + 20 * self.pubkeys_cnt + 65 * self.threshold) as usize
    }
    pub fn generator_key(
        pubkeys_cnt: u8,
        threshold: u8,
        require_first_n: u8,
    ) -> (Vec<u8>, Vec<Privkey>) {
        let mut pubkey_data = BytesMut::with_capacity(pubkeys_cnt as usize * 20 + 4);
        pubkey_data.put_u8(0);
        pubkey_data.put_u8(require_first_n);
        pubkey_data.put_u8(threshold);
        pubkey_data.put_u8(pubkeys_cnt);

        let mut pubkey_hashs: Vec<Privkey> = Vec::new();
        for _i in 0..pubkeys_cnt {
            let privkey = Generator::random_privkey();
            let hash = CKbAuth::get_ckb_pub_key_hash(&privkey);
            pubkey_hashs.push(privkey);
            pubkey_data.put(Bytes::from(hash));
        }
        (pubkey_data.freeze().to_vec(), pubkey_hashs)
    }

    pub fn multickb_sign(&self, msg: &H256) -> Bytes {
        let mut sign_data = BytesMut::with_capacity(self.get_mulktisig_size());
        sign_data.put(Bytes::from(self.pubkey_data.clone()));
        let privkey_size = self.privkeys.len();
        for i in 0..self.threshold {
            if privkey_size > i as usize {
                sign_data.put(CKbAuth::ckb_sign(msg, &self.privkeys[i as usize]));
            } else {
                sign_data.put(CKbAuth::ckb_sign(msg, &self.privkeys[privkey_size - 1]));
            }
        }
        sign_data.freeze()
    }

    pub fn new(pubkeys_cnt: u8, threshold: u8, require_first_n: u8) -> Box<CkbMultisigAuth> {
        let (pubkey_data, privkeys) =
            CkbMultisigAuth::generator_key(pubkeys_cnt, threshold, require_first_n);
        let hash = ckb_hash::blake2b_256(&pubkey_data);

        Box::new(CkbMultisigAuth {
            pubkeys_cnt,
            threshold,
            pubkey_data,
            privkeys,
            hash: hash[0..20].to_vec(),
        })
    }
}
impl Auth for CkbMultisigAuth {
    fn get_pub_key_hash(&self) -> Vec<u8> {
        self.hash.clone()
    }
    fn get_algorithm_type(&self) -> u8 {
        AlgorithmType::CkbMultisig as u8
    }
    fn sign(&self, msg: &H256) -> Bytes {
        self.multickb_sign(msg)
    }
    fn get_sign_size(&self) -> usize {
        self.get_mulktisig_size()
    }
}

#[derive(Clone)]
pub struct SchnorrAuth {
    pub privkey: secp256k1::SecretKey,
    pub pubkey: secp256k1::PublicKey,
}
impl SchnorrAuth {
    pub fn new() -> Box<dyn Auth> {
        let generator: secp256k1::Secp256k1<secp256k1::All> = secp256k1::Secp256k1::new();
        let mut rng = thread_rng();
        let (privkey, pubkey) = generator.generate_keypair(&mut rng);
        Box::new(SchnorrAuth { privkey, pubkey })
    }
}
impl Auth for SchnorrAuth {
    fn get_pub_key_hash(&self) -> Vec<u8> {
        let secp: secp256k1::Secp256k1<secp256k1::All> = secp256k1::Secp256k1::gen_new();
        let key_pair = secp256k1::KeyPair::from_secret_key(&secp, self.privkey);
        let xonly = secp256k1::XOnlyPublicKey::from_keypair(&key_pair).serialize();

        Vec::from(&ckb_hash::blake2b_256(xonly)[..20])
    }
    fn get_algorithm_type(&self) -> u8 {
        AlgorithmType::SchnorrOrTaproot as u8
    }
    fn get_sign_size(&self) -> usize {
        32 + 64
    }
    fn sign(&self, msg: &H256) -> Bytes {
        let secp: secp256k1::Secp256k1<secp256k1::All> = secp256k1::Secp256k1::gen_new();
        let secp_msg = secp256k1::Message::from_slice(msg.as_bytes()).unwrap();
        let key_pair = secp256k1::KeyPair::from_secret_key(&secp, self.privkey);
        let sign = secp.sign_schnorr_no_aux_rand(&secp_msg, &key_pair);

        let mut ret = BytesMut::with_capacity(32 + 64);
        let xonly = secp256k1::XOnlyPublicKey::from_keypair(&key_pair)
            .serialize()
            .to_vec();
        ret.put(Bytes::from(xonly.clone()));
        ret.put(Bytes::from(sign.as_ref().to_vec()));
        ret.freeze()
    }
}

#[derive(Clone)]
struct RSAAuth {
    pub pri_key: Vec<u8>,
    pub pub_key: Vec<u8>,
}
impl RSAAuth {
    fn new() -> Box<dyn Auth> {
        let bits = 1024;
        let exponent = 65537;

        use mbedtls::pk::Pk;
        use mbedtls::rng::ctr_drbg::CtrDrbg;

        let mut rng =
            CtrDrbg::new(Arc::new(mbedtls::rng::OsEntropy::new()), None).expect("new ctrdrbg rng");
        let mut rsa_key = Pk::generate_rsa(&mut rng, bits, exponent).expect("generate rsa");

        let pri_key = {
            let mut buf = [0u8; 1024 * 4];
            let r = rsa_key
                .write_private_der(&mut buf)
                .expect("export private key")
                .unwrap();
            r.to_vec()
        };

        let pub_key = {
            let mut buf = [0u8; 1024 * 4];
            let r = rsa_key
                .write_public_der(&mut buf)
                .expect("export public key")
                .unwrap();
            r.to_vec()
        };

        Box::new(RSAAuth { pri_key, pub_key })
    }
    fn rsa_sign(msg: &H256, privkey: &[u8], pubkey: &[u8]) -> Bytes {
        let mut sig = Vec::<u8>::new();
        sig.push(1); // algorithm id
        sig.push(1); // key size, 1024
        sig.push(0); // padding, PKCS# 1.5
        sig.push(6); // hash type SHA256

        let (e, n) = Self::get_e_n(pubkey);
        sig.extend_from_slice(&e); // 4 bytes E
        sig.extend_from_slice(&n); // N
        sig.extend_from_slice(&Self::rsa_sign_msg(msg, privkey));

        Bytes::from(sig.clone())
    }
    fn get_e_n(pub_key: &[u8]) -> (Vec<u8>, Vec<u8>) {
        use mbedtls::pk::Pk;
        let pub_key = Pk::from_public_key(pub_key).expect("");
        let mut e = pub_key
            .rsa_public_exponent()
            .expect("rsa exponent")
            .to_le_bytes()
            .to_vec();
        let mut n = pub_key
            .rsa_public_modulus()
            .expect("rsa modulus")
            .to_binary()
            .unwrap();
        n.reverse();

        while e.len() < 4 {
            e.push(0);
        }
        while n.len() < 128 {
            n.push(0);
        }

        (e, n)
    }
    fn rsa_sign_msg(msg: &H256, privkey: &[u8]) -> Vec<u8> {
        use mbedtls::hash::Type::Sha256;
        use mbedtls::pk::{Options, Pk, RsaPadding};
        use mbedtls::rng::ctr_drbg::CtrDrbg;

        let mut priv_key = Pk::from_private_key(privkey, None).expect("import rsa private key");
        priv_key.set_options(Options::Rsa {
            padding: RsaPadding::Pkcs1V15,
        });
        let mut rng = CtrDrbg::new(Arc::new(mbedtls::rng::OsEntropy::new()), None)
            .expect("generate ctr drbg");
        let mut signature = [0u8; 1024];

        let mut md_hash = mbedtls::hash::Md::new(Sha256).expect("new sha256");
        md_hash.update(msg.as_bytes()).expect("update sha256");
        let mut sign_hash = [0u8; 32];
        md_hash.finish(&mut sign_hash).expect("sha256 finish");

        let size = priv_key
            .sign(Sha256, &sign_hash, &mut signature, &mut rng)
            .expect("rsa sign");
        let signature = signature[..size].to_vec();
        signature
    }
}
impl Auth for RSAAuth {
    fn get_sign_size(&self) -> usize {
        264
    }
    fn get_pub_key_hash(&self) -> Vec<u8> {
        let (e, n) = Self::get_e_n(&self.pub_key);

        let mut sig = Vec::<u8>::new();
        sig.push(1); // algorithm id
        sig.push(1); // key size, 1024
        sig.push(0); // padding, PKCS# 1.5
        sig.push(6); // hash type SHA256

        sig.extend_from_slice(&e);
        sig.extend_from_slice(&n);

        let hash = ckb_hash::blake2b_256(sig.as_slice());

        hash[0..20].to_vec()
    }
    fn get_algorithm_type(&self) -> u8 {
        AlgorithmType::RSA as u8
    }
    fn sign(&self, msg: &H256) -> Bytes {
        RSAAuth::rsa_sign(msg, &self.pri_key, &self.pub_key)
    }
}

#[derive(Clone)]
struct OwnerLockAuth {}
impl OwnerLockAuth {
    fn new() -> Box<dyn Auth> {
        Box::new(OwnerLockAuth {})
    }
}
impl Auth for OwnerLockAuth {
    fn get_pub_key_hash(&self) -> Vec<u8> {
        let hash = CellOutput::calc_data_hash(&ALWAYS_SUCCESS);
        let hash = hash.as_slice().to_vec();
        _dbg_print_mem(&hash, "cell hash");
        hash[0..20].to_vec()
    }
    fn get_algorithm_type(&self) -> u8 {
        AlgorithmType::OwnerLock as u8
    }
    fn sign(&self, _msg: &H256) -> Bytes {
        Bytes::from([0; 64].to_vec())
    }
}

pub fn gen_tx_scripts_verifier(
    tx: TransactionView,
    data_loader: DummyDataLoader,
) -> TransactionScriptsVerifier<DummyDataLoader> {
    let resolved_tx = build_resolved_tx(&data_loader, &tx);
    let mut verifier = TransactionScriptsVerifier::new(
        Arc::new(resolved_tx),
        data_loader.clone(),
        Arc::new(ConsensusBuilder::default().build()),
        Arc::new(TxVerifyEnv::new_commit(
            &HeaderView::new_advanced_builder().build(),
        )),
    );
    verifier.set_debug_printer(debug_printer);
    verifier
}
