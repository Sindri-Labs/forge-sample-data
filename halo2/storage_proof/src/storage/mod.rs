use crate::{
    block_header::{
        EthBlockHeaderChip, EthBlockHeaderTrace, EthBlockHeaderTraceWitness,
        GOERLI_BLOCK_HEADER_RLP_MAX_BYTES, MAINNET_BLOCK_HEADER_RLP_MAX_BYTES, self,
    },
    halo2_proofs::{
        circuit::{Layouter, SimpleFloorPlanner, Value},
        plonk::{Circuit, ConstraintSystem, Error},
    },
    mpt::{AssignedBytes, MPTFixedKeyInput, MPTFixedKeyProof, MPTFixedKeyProofWitness},
    rlp::{rlc::RlcTrace, RlpArrayTraceWitness, RlpFieldTraceWitness},
    util::{
        bytes_be_to_u128, bytes_be_to_uint, bytes_be_var_to_fixed, encode_addr_to_field,
        encode_h256_to_field, encode_u256_to_field, uint_to_bytes_be, AssignedH256,
        EthConfigParams,
    },
    EthChip, EthConfig, Field, Network,
};
#[cfg(feature = "display")]
use ark_std::{end_timer, start_timer};
use ethers_core::types::{Address, Block, H256, U256};
#[cfg(feature = "providers")]
use ethers_providers::{Http, Provider};
use halo2_base::{gates::GateInstructions, AssignedValue, Context, ContextParams, SKIP_FIRST_PASS};
use itertools::Itertools;
use snark_verifier_sdk::CircuitExt;
use std::marker::PhantomData;

#[cfg(all(test, feature = "providers"))]
mod tests;

#[derive(Clone, Debug)]
pub struct EthAccountTrace<'v, F: Field> {
    pub nonce_trace: RlcTrace<'v, F>,
    pub balance_trace: RlcTrace<'v, F>,
    pub storage_root_trace: RlcTrace<'v, F>,
    pub code_hash_trace: RlcTrace<'v, F>,
}

#[derive(Clone, Debug)]
pub struct EthAccountTraceWitness<'v, F: Field> {
    array_witness: RlpArrayTraceWitness<'v, F>,
    mpt_witness: MPTFixedKeyProofWitness<'v, F>,
}

#[derive(Clone, Debug)]
pub struct EthStorageTrace<'v, F: Field> {
    pub value_bytes: AssignedBytes<'v, F>,
}

#[derive(Clone, Debug)]
pub struct EthStorageTraceWitness<'v, F: Field> {
    value_witness: RlpFieldTraceWitness<'v, F>,
    mpt_witness: MPTFixedKeyProofWitness<'v, F>,
}

#[derive(Clone, Debug)]
pub struct EthBlockAccountStorageTrace<'v, F: Field> {
    pub block_trace: EthBlockHeaderTrace<'v, F>,
    pub acct_trace: EthAccountTrace<'v, F>,
    pub storage_trace: Vec<EthStorageTrace<'v, F>>,
    pub digest: EIP1186ResponseDigest<'v, F>,
}

#[derive(Clone, Debug)]
pub struct EthBlockAccountStorageTraceWitness<'v, F: Field> {
    block_witness: EthBlockHeaderTraceWitness<'v, F>,
    acct_witness: EthAccountTraceWitness<'v, F>,
    storage_witness: Vec<EthStorageTraceWitness<'v, F>>,
    digest: EIP1186ResponseDigest<'v, F>,
}

#[derive(Clone, Debug)]
pub struct EIP1186ResponseDigest<'v, F: Field> {
    pub block_hash: AssignedH256<'v, F>,
    pub block_number: AssignedValue<'v, F>,
    pub address: AssignedValue<'v, F>,
    // the value U256 is interpreted as H256 (padded with 0s on left)
    pub slots_values: Vec<(AssignedH256<'v, F>, AssignedH256<'v, F>)>,
}

pub trait EthStorageChip<'v, F: Field> {
    fn parse_account_proof_phase0(
        &mut self,
        ctx: &mut Context<'v, F>,
        state_root_bytes: &[AssignedValue<'v, F>],
        addr: AssignedBytes<'v, F>,
        proof: MPTFixedKeyProof<'v, F>,
    ) -> EthAccountTraceWitness<'v, F>;

    fn parse_account_proof_phase1(
        &mut self,
        ctx: &mut Context<'v, F>,
        witness: EthAccountTraceWitness<'v, F>,
    ) -> EthAccountTrace<'v, F>;

    fn parse_storage_proof_phase0(
        &mut self,
        ctx: &mut Context<'v, F>,
        storage_root_bytes: &[AssignedValue<'v, F>],
        slot_bytes: AssignedBytes<'v, F>,
        proof: MPTFixedKeyProof<'v, F>,
    ) -> EthStorageTraceWitness<'v, F>;

    fn parse_storage_proof_phase1(
        &mut self,
        ctx: &mut Context<'v, F>,
        witness: EthStorageTraceWitness<'v, F>,
    ) -> EthStorageTrace<'v, F>;

    fn parse_eip1186_proofs_phase0(
        &mut self,
        ctx: &mut Context<'v, F>,
        state_root_bytes: &[AssignedValue<'v, F>],
        addr: AssignedBytes<'v, F>,
        acct_pf: MPTFixedKeyProof<'v, F>,
        storage_pfs: Vec<(AssignedBytes<'v, F>, MPTFixedKeyProof<'v, F>)>, // (slot_bytes, storage_proof)
    ) -> (EthAccountTraceWitness<'v, F>, Vec<EthStorageTraceWitness<'v, F>>);

    fn parse_eip1186_proofs_phase1(
        &mut self,
        ctx: &mut Context<'v, F>,
        witness: (EthAccountTraceWitness<'v, F>, Vec<EthStorageTraceWitness<'v, F>>),
    ) -> (EthAccountTrace<'v, F>, Vec<EthStorageTrace<'v, F>>);

    // slot and block_hash are big-endian 16-byte
    // inputs have H256 represented in (hi,lo) format as two u128s
    // block number and slot values can be derived from the final trace output
    fn parse_eip1186_proofs_from_block_phase0(
        &mut self,
        ctx: &mut Context<'v, F>,
        input: EthBlockStorageInputAssigned<'v, F>,
        network: Network,
    ) -> EthBlockAccountStorageTraceWitness<'v, F>
    where
        Self: EthBlockHeaderChip<'v, F>;

    fn parse_eip1186_proofs_from_block_phase1(
        &mut self,
        ctx: &mut Context<'v, F>,
        witness: EthBlockAccountStorageTraceWitness<'v, F>,
    ) -> EthBlockAccountStorageTrace<'v, F>
    where
        Self: EthBlockHeaderChip<'v, F>;
}

impl<'v, F: Field> EthStorageChip<'v, F> for EthChip<'v, F> {
    fn parse_account_proof_phase0(
        &mut self,
        ctx: &mut Context<'v, F>,
        state_root_bytes: &[AssignedValue<'v, F>],
        addr: AssignedBytes<'v, F>,
        proof: MPTFixedKeyProof<'v, F>,
    ) -> EthAccountTraceWitness<'v, F> {
        assert_eq!(32, proof.key_byte_len);

        // check key is keccak(addr)
        assert_eq!(addr.len(), 20);
        let hash_query_idx = self.mpt.keccak.keccak_fixed_len(ctx, self.mpt.rlp.gate(), addr, None);
        let hash_bytes = &self.keccak().fixed_len_queries[hash_query_idx].output_assigned;

        for (hash, key) in hash_bytes.iter().zip(proof.key_bytes.iter()) {
            ctx.constrain_equal(hash, key);
        }

        // check MPT root is state root
        for (pf_root, root) in proof.root_hash_bytes.iter().zip(state_root_bytes.iter()) {
            ctx.constrain_equal(pf_root, root);
        }

        // parse value RLP([nonce, balance, storage_root, code_hash])
        let array_witness = self.mpt.rlp.decompose_rlp_array_phase0(
            ctx,
            proof.value_bytes.clone(),
            &[33, 13, 33, 33],
            false,
        );
        // Check MPT inclusion for:
        // keccak(addr) => RLP([nonce, balance, storage_root, code_hash])
        let max_depth = proof.max_depth;
        let mpt_witness =
            self.mpt.parse_mpt_inclusion_fixed_key_phase0(ctx, proof, 32, 114, max_depth);

        EthAccountTraceWitness { array_witness, mpt_witness }
    }

    fn parse_account_proof_phase1(
        &mut self,
        ctx: &mut Context<'v, F>,
        witness: EthAccountTraceWitness<'v, F>,
    ) -> EthAccountTrace<'v, F> {
        self.mpt.parse_mpt_inclusion_fixed_key_phase1(ctx, witness.mpt_witness);
        let array_trace: [_; 4] = self
            .mpt
            .rlp
            .decompose_rlp_array_phase1(ctx, witness.array_witness, false)
            .field_trace
            .try_into()
            .unwrap();
        let [nonce_trace, balance_trace, storage_root_trace, code_hash_trace] =
            array_trace.map(|trace| trace.field_trace);
        EthAccountTrace { nonce_trace, balance_trace, storage_root_trace, code_hash_trace }
    }

    fn parse_storage_proof_phase0(
        &mut self,
        ctx: &mut Context<'v, F>,
        storage_root_bytes: &[AssignedValue<'v, F>],
        slot: AssignedBytes<'v, F>,
        proof: MPTFixedKeyProof<'v, F>,
    ) -> EthStorageTraceWitness<'v, F> {
        assert_eq!(32, proof.key_byte_len);

        // check key is keccak(slot)
        let hash_query_idx = self.mpt.keccak.keccak_fixed_len(ctx, self.mpt.rlp.gate(), slot, None);
        let hash_bytes = &self.keccak().fixed_len_queries[hash_query_idx].output_assigned;

        for (hash, key) in hash_bytes.iter().zip(proof.key_bytes.iter()) {
            ctx.constrain_equal(hash, key);
        }
        // check MPT root is storage_root
        for (pf_root, root) in proof.root_hash_bytes.iter().zip(storage_root_bytes.iter()) {
            ctx.constrain_equal(pf_root, root);
        }

        // parse slot value
        let value_witness =
            self.mpt.rlp.decompose_rlp_field_phase0(ctx, proof.value_bytes.clone(), 32);
        // check MPT inclusion
        let max_depth = proof.max_depth;
        let mpt_witness =
            self.mpt.parse_mpt_inclusion_fixed_key_phase0(ctx, proof, 32, 33, max_depth);

        EthStorageTraceWitness { value_witness, mpt_witness }
    }

    fn parse_storage_proof_phase1(
        &mut self,
        ctx: &mut Context<'v, F>,
        witness: EthStorageTraceWitness<'v, F>,
    ) -> EthStorageTrace<'v, F> {
        self.mpt.parse_mpt_inclusion_fixed_key_phase1(ctx, witness.mpt_witness);
        let value_trace = self.mpt.rlp.decompose_rlp_field_phase1(ctx, witness.value_witness);
        let value_bytes = value_trace.field_trace.values;
        debug_assert_eq!(value_bytes.len(), 32);
        EthStorageTrace { value_bytes }
    }

    fn parse_eip1186_proofs_phase0(
        &mut self,
        ctx: &mut Context<'v, F>,
        state_root: &[AssignedValue<'v, F>],
        addr: AssignedBytes<'v, F>,
        acct_pf: MPTFixedKeyProof<'v, F>,
        storage_pfs: Vec<(AssignedBytes<'v, F>, MPTFixedKeyProof<'v, F>)>, // (slot_bytes, storage_proof)
    ) -> (EthAccountTraceWitness<'v, F>, Vec<EthStorageTraceWitness<'v, F>>) {
        let acct_trace = self.parse_account_proof_phase0(ctx, state_root, addr, acct_pf);
        let storage_root = &acct_trace.array_witness.field_witness[2].field_cells;

        let storage_trace = storage_pfs
            .into_iter()
            .map(|(slot, storage_pf)| {
                self.parse_storage_proof_phase0(ctx, storage_root, slot, storage_pf)
            })
            .collect();

        (acct_trace, storage_trace)
    }

    fn parse_eip1186_proofs_phase1(
        &mut self,
        ctx: &mut Context<'v, F>,
        (acct_witness, storage_witness): (
            EthAccountTraceWitness<'v, F>,
            Vec<EthStorageTraceWitness<'v, F>>,
        ),
    ) -> (EthAccountTrace<'v, F>, Vec<EthStorageTrace<'v, F>>) {
        let acct_trace = self.parse_account_proof_phase1(ctx, acct_witness);
        let storage_trace = storage_witness
            .into_iter()
            .map(|storage_witness| self.parse_storage_proof_phase1(ctx, storage_witness))
            .collect();
        (acct_trace, storage_trace)
    }

    fn parse_eip1186_proofs_from_block_phase0(
        &mut self,
        ctx: &mut Context<'v, F>,
        input: EthBlockStorageInputAssigned<'v, F>,
        network: Network,
    ) -> EthBlockAccountStorageTraceWitness<'v, F>
    where
        Self: EthBlockHeaderChip<'v, F>,
    {
        // check block_hash
        // TODO: more optimal to compute the `block_hash` via keccak below and then just constrain the bytes match this (hi,lo) representation
        let block_hash = input.block_hash;
        let address = input.storage.address;
        let block_hash_bytes0 =
            block_hash.iter().map(|u128| uint_to_bytes_be(ctx, self.range(), u128, 16)).concat();
        let mut block_header = input.block_header;
        let max_len = match network {
            Network::Goerli => GOERLI_BLOCK_HEADER_RLP_MAX_BYTES,
            Network::Mainnet => MAINNET_BLOCK_HEADER_RLP_MAX_BYTES,
        };
        block_header.resize(max_len, 0);
        let block_witness = self.decompose_block_header_phase0(ctx, &block_header, network);

        let state_root = &block_witness.rlp_witness.field_witness[3].field_cells;
        let block_hash_bytes1 =
            &self.keccak().var_len_queries[block_witness.block_hash_query_idx].output_assigned;
        for (byte0, byte1) in block_hash_bytes0.iter().zip(block_hash_bytes1.iter()) {
            ctx.constrain_equal(byte0, byte1);
        }

        // compute block number from big-endian bytes
        let block_num_bytes = &block_witness.rlp_witness.field_witness[8].field_cells;
        let block_num_len = &block_witness.rlp_witness.field_witness[8].field_len;
        let block_number =
            bytes_be_var_to_fixed(ctx, self.gate(), block_num_bytes, block_num_len, 4);
        let block_number = bytes_be_to_uint(ctx, self.gate(), &block_number, 4);

        // verify account + storage proof
        let addr_bytes = uint_to_bytes_be(ctx, self.range(), &address, 20);
        let acct_witness = self.parse_account_proof_phase0(
            ctx,
            state_root,
            addr_bytes.clone(),
            input.storage.acct_pf,
        );
        let storage_root = &acct_witness.array_witness.field_witness[2].field_cells;

        let mut slots_values = Vec::with_capacity(input.storage.storage_pfs.len());
        let storage_witness = input
            .storage
            .storage_pfs
            .into_iter()
            .map(|(slot, storage_pf)| {
                let slot_bytes =
                    slot.iter().map(|u128| uint_to_bytes_be(ctx, self.range(), u128, 16)).concat();
                let witness =
                    self.parse_storage_proof_phase0(ctx, storage_root, slot_bytes, storage_pf);
                // get value as U256 from RLP decoding, convert to H256, then to hi-lo
                let value_bytes = &witness.value_witness.witness.field_cells;
                let value_len = &witness.value_witness.witness.field_len;
                let value_bytes =
                    bytes_be_var_to_fixed(ctx, self.gate(), value_bytes, value_len, 32);
                let value: [_; 2] =
                    bytes_be_to_u128(ctx, self.gate(), &value_bytes).try_into().unwrap();
                slots_values.push((slot, value));

                witness
            })
            .collect();
        EthBlockAccountStorageTraceWitness {
            block_witness,
            acct_witness,
            storage_witness,
            digest: EIP1186ResponseDigest { block_hash, block_number, address, slots_values },
        }
    }

    fn parse_eip1186_proofs_from_block_phase1(
        &mut self,
        ctx: &mut Context<'v, F>,
        witness: EthBlockAccountStorageTraceWitness<'v, F>,
    ) -> EthBlockAccountStorageTrace<'v, F>
    where
        Self: EthBlockHeaderChip<'v, F>,
    {
        let block_trace = self.decompose_block_header_phase1(ctx, witness.block_witness);
        let (acct_trace, storage_trace) =
            self.parse_eip1186_proofs_phase1(ctx, (witness.acct_witness, witness.storage_witness));
        EthBlockAccountStorageTrace {
            block_trace,
            acct_trace,
            storage_trace,
            digest: witness.digest,
        }
    }
}

#[derive(Clone, Debug)]
pub struct EthStorageInput {
    pub addr: Address,
    pub acct_pf: MPTFixedKeyInput,
    pub storage_pfs: Vec<(H256, U256, MPTFixedKeyInput)>, // (slot, value, proof)
}

#[derive(Clone, Debug)]
pub struct EthBlockStorageInput {
    pub block: Block<H256>,
    pub block_number: u32,
    pub block_hash: H256,
    pub block_header: Vec<u8>,
    pub storage: EthStorageInput,
}

impl EthStorageInput {
    pub fn assign<'v, F: Field>(
        &self,
        ctx: &mut Context<'_, F>,
        gate: &impl GateInstructions<F>,
    ) -> EthStorageInputAssigned<'v, F> {
        let address = encode_addr_to_field(&self.addr);
        let address = gate.load_witness(ctx, Value::known(address));
        let acct_pf = self.acct_pf.assign(ctx, gate);
        let storage_pfs = self
            .storage_pfs
            .iter()
            .map(|(slot, _, pf)| {
                let slot = encode_h256_to_field(slot);
                let slot = slot.map(|slot| gate.load_witness(ctx, Value::known(slot)));
                let pf = pf.assign(ctx, gate);
                (slot, pf)
            })
            .collect();
        EthStorageInputAssigned { address, acct_pf, storage_pfs }
    }
}

impl EthBlockStorageInput {
    pub fn assign<'v, F: Field>(
        &self,
        ctx: &mut Context<'_, F>,
        gate: &impl GateInstructions<F>,
    ) -> EthBlockStorageInputAssigned<'v, F> {
        let block_hash = encode_h256_to_field(&self.block_hash);
        let block_hash =
            block_hash.map(|block_hash| gate.load_witness(ctx, Value::known(block_hash)));
        let storage = self.storage.assign(ctx, gate);
        EthBlockStorageInputAssigned {
            block_hash,
            block_header: self.block_header.clone(),
            storage,
        }
    }
}

#[derive(Clone, Debug)]
pub struct EthStorageInputAssigned<'v, F: Field> {
    pub address: AssignedValue<'v, F>, // U160
    pub acct_pf: MPTFixedKeyProof<'v, F>,
    pub storage_pfs: Vec<(AssignedH256<'v, F>, MPTFixedKeyProof<'v, F>)>, // (slot, proof) where slot is H256 as (u128, u128)
}


#[derive(Clone, Debug)]
pub struct EthBlockStorageInputAssigned<'v, F: Field> {
    pub block_hash: AssignedH256<'v, F>, // H256 as (u128, u128)
    pub block_header: Vec<u8>,
    pub storage: EthStorageInputAssigned<'v, F>,
}

#[derive(Clone, Debug)]
pub struct EthBlockStorageCircuit<F> {
    pub inputs: EthBlockStorageInput,
    network: Network,
    _marker: PhantomData<F>,
}

impl<F: Field> EthBlockStorageCircuit<F> {
    #[cfg(feature = "providers")]
    pub fn from_provider(
        provider: &Provider<Http>,
        block_number: u32,
        address: Address,
        slots: Vec<H256>,
        acct_pf_max_depth: usize,
        storage_pf_max_depth: usize,
        network: Network,
    ) -> Self {
        use crate::providers::get_block_storage_input;

        let inputs = get_block_storage_input(
            provider,
            block_number,
            address,
            slots,
            acct_pf_max_depth,
            storage_pf_max_depth,
        );
        Self { inputs, network, _marker: PhantomData }
    }

    // blockHash, blockNumber, address, (slot, value)s
    // with H256 encoded as hi-lo (u128, u128)
    pub fn instance(&self) -> Vec<F> {
        let EthBlockStorageInput { block_number, block_hash, storage, .. } = &self.inputs;
        let EthStorageInput { addr, storage_pfs, .. } = storage;
        let mut instance = Vec::with_capacity(4 + 4 * storage_pfs.len());
        instance.extend(encode_h256_to_field::<F>(block_hash));
        instance.push(F::from(*block_number as u64));
        instance.push(encode_addr_to_field(addr));
        for (slot, value, _) in storage_pfs {
            instance.extend(encode_h256_to_field::<F>(slot));
            instance.extend(encode_u256_to_field::<F>(value));
        }
        instance
    }

    pub fn from_json(
        json_loc: &str,
    ) -> Self {
        use crate::providers::saved_block_storage_input;

        let inputs = saved_block_storage_input(json_loc);
        let network = Network::Mainnet;

        Self { inputs, network, _marker: PhantomData }
    }

}


impl<F: Field> Default for EthBlockStorageCircuit<F> {
    fn default() -> Self {
        let s = r#"
        {
            "block": {
              "hash": "0xf152ad7de1411489dd7bd38d958f1c826f3e98b348c77a2141cef101d6e2dbde",
              "parentHash": "0xc0d33af36f33cc4324e40b2e813e2114a8138fb168b8d2f25484d57a00ba0c41",
              "sha3Uncles": "0x1dcc4de8dec75d7aab85b567b6ccd41ad312451b948a7413f0a142fd40d49347",
              "miner": "0x690b9a9e9aa1c9db991c7721a92d351db4fac990",
              "stateRoot": "0xf8bef79a5edec709ff3d26126cbb2a8aa0da5ff80f6e3ffa16ec2c5d88ffab6f",
              "transactionsRoot": "0xef9762a488e6261436afac56eebfc793a2364e8b62e301a3115a0ca034870671",
              "receiptsRoot": "0xa39501c87cfe87e85aa510de8a4f14ee0662c5ee0dc4e38bc625d61913a67348",
              "number": "0xf993fe",
              "gasUsed": "0x145d078",
              "gasLimit": "0x1c9c380",
              "extraData": "0x406275696c64657230783639",
              "logsBloom": "0x9b3cf0e2f3b18e75f19205758144332109d80ece8a2c3a0cf1f181d57405d11ed2efd741c64eec6e2a0e3c494bf185b616e176cd7a07f873864ec2ffdc236c7d9a16d5b8f370cd6e7babbeefe63061e3f2d36ae9174d965389f4feed8e063ca03344f3590f0a55be5ce19ba516a66cf5cc3ea955e07c5d519e24d9d6dd5e42d5b243eb1bfa223100e5c87fc61ae0cbc16cea0c99cdfe0549c9ac6b7695d6bbee9b8aade3f3ef6a49bc1348d506a2ac476e70722a251ffba7596d5e5a124fbe3c491d5442c188e196705f188d1a2f124ed7ecf3dd9ece65d786643fa73b2faf59f138239915a70a20740cfbc042837e162c56c1e31b782f636ed5ea1816945a45",
              "timestamp": "0x63b9a977",
              "difficulty": "0x0",
              "totalDifficulty": "0xc70d815d562d3cfa955",
              "sealFields": [],
              "uncles": [],
              "transactions": [
                "0x3a35a800b4ce1d53a83f11eff56572ff9c2b356eb37fd325722b6ce285082719",
                "0x234e7b62e0d3f81b0b831a6c0bff563369686b537bc5fe8b6553bb091cf6d01f",
                "0xd86f53a43ac74642a1e7ad0ceff029336e029a1ff8fcbc5fe7714c670ba1be98",
                "0xc9b775451fa61db1958f11b2d317e2f070f12607dcb39b547924ace2c8c496a7",
                "0x78b05b23dbaab3bd428110aca966adac82402363ca4a63926771bab3ed127655",
                "0xea2e39f6e6ddcba14f685b5bfcb4fb832a31670d9a54eab4e5097afd0441fef0",
                "0xc55ae36ecccefb1d05a40b8d20064d3a04c6651edd6acd2c3748ebb2fe40f5d6",
                "0x3290f24e3bec0d37cc6c2f6dc46a6297d784793fe79cada16259387bb8f5b480",
                "0x0893df3f56ecfcb8a34b134ae5de9cc0c8b76f9eabd6b12d9ce048daf66f2cca",
                "0x81591e275b9c5e6867df597d60bd31fa69bccd9153f89efeea8562546f4dfa77",
                "0x024062ca04b003dccc8fc1430dd284bfda676d7d9b47923b5442d09a2ffa6ff3",
                "0x6fa2546a916dead4b94c1ce322b3bc395d298445b01def9b3f653144ad029260",
                "0xafc10de17a6f2a3b22eedadca27d2fd9187667ef94ef3a9edcc738ca00359367",
                "0x876769930e79b8b6110c3cb3d4fa8ce0917b1d770da771976f6a206fb59ae03a",
                "0xe9715f368ddbd6443e37fdc2bab5683a58965db1861cce6e8e211a52c6aef3b4",
                "0xd8c40052ae29bb7a5e86081aecb2306ed952c56217aef7c0965cebf4097cb594",
                "0x297756d0ae51dade27ab59c0ee2fa4704c11f9ee4191249bd79cbab41dfec2a4",
                "0x83bcd41a02e6e0747b81ccb475c0e6786f7d38d49d747581f83a68acc60f8396",
                "0x8e12d4415a605007220fc07ebcb9a69445d92a495192026b6e03af957f5c95e2",
                "0x4e96883b8780a511c894598a0265fbd1633e271c136f8112faafd281596b8935",
                "0x43015963ba62321cd860eb017a6b556b2c378ef3771bc236da6b64e7f0e8c7c3",
                "0x8eafb2edbd429cdf11e5348125e38ea09015a60354f5d71cdff9c15326eb7ccd",
                "0x2d10fbc547551dfaf8a85defb7c4220c60666ea887dfb9466829694ea2f3ea95",
                "0xa7f2965f486073dc291936c6a9018e78fda2f1f07b8e7f23f11c8e49c1ebe8a0",
                "0xd8dba01b33224b7a1f3b65dce271fdf3e23c960ff5c2f3533e1bd4244e9de442",
                "0x4b732e28b00dee58a25dc47dcab4165cd8f5245da050e6368c819e7234109d0e",
                "0x797d442c48442bb1b13c72123f7a23af21b350f13f08143fb9cb3721d2c05c30",
                "0x47c1800297df3c6358b46c8d130a1ed029d85b3b4cd5416774f6e2bb08c05447",
                "0xf3e323f44ba16b682c2671aba7f70f55c35d29c56f3d9f76fc2acd9f0747921d",
                "0xaaafb6ae018c372b6102f003472ee90b150cfa4e7e56d40c62c1aa5b36a00660",
                "0x155a2fb10cd711ad0819edf70be119499ec66ab7c4bd6ac492ee540dc233ded7",
                "0xc4c1007e5eb6557a1dbd6b52c7242e36421fd22b64c9fcb9b63975f2d1645f59",
                "0xf0c756170243a948132cfeef8aaba33c000267f95d58f43236b49d11a0fcc831",
                "0x307ccbccc6dd836b73cf89c2478f8fb466f18e7624ca013da8a73561bbb1ff25",
                "0x5e9df12dc0cb21ae934d049a617c9fb1bcb74169abf027672d2939fd098afb8d",
                "0x5da1557fca1ef8797142eec112cce0add2dcff507397027c4aa17817721728f9",
                "0xc95851b270ac48b082d71706c72b9247882696974e88b3a4b52fad6a1df361be",
                "0x5a3558341f16235a41e7c7e6f715aa0dd5b5c1ae21b1c9266704c10d5fe67a7c",
                "0xd21f29c59161c77389eefc8ffd5e0a93787b31af2df070d6ae2523ecffc4277e",
                "0x713f00db9e2787515b0bb625d48a37d22beb7bc086120a2a043ec74e365d2cee",
                "0xa9318b2b5b384053ea5c0d02fe0eb30bdcff7d6790ca59b4290ac90827997a17",
                "0x7572099ba7cd584adea695e5ce9ab27312ace1233f60b1197010d4fd208bd4d1",
                "0xc1d985c5e8911540dda3b9e17ed9b40e4c96c192f3b252a2cfd223801d540777",
                "0xdbafb7bb8860e6d562c317d1e245ce60bdd4fa519dc85c33c0b428cff1817e3d",
                "0x2d15a33d8ee38c462568426d09bfbf9f3f4d4f38059ea6a578876be9a4d23fe4",
                "0xc75716461acfa03ba38ea8b527bce604bf95f1d388b33463a172cf89027c317e",
                "0x5c441da90e11753fce4aeb3fdcd0665eeee4f4c097ddc15a6644eda8292248f7",
                "0xdd46fe564baa607f04b930153db531d5726e448045f3ff8797e854c8744459f0",
                "0xf9a17f240a6ab30cf40a85c1998d3267bb5ec0fc0b938b745f2763ebdac4cbdf",
                "0x17429841b29fe48b6c3a9f225448ad22c34ce5dd5c74f46e466db3379ec09865",
                "0xc643aaeb1322b3b2efa02d7992fe6bfbe073d72260415edee20f1cc230c1ecc9",
                "0x2d5b926a4ae24efc2a0bac2701f078db6ff6554747e18fefc379e60619d9b56f",
                "0x44d08ff0457201def40797dc1fd087692b2bd69e2173733396ca32a14275413e",
                "0x639c2b00cecb3449c9fad78e37c7f3de84537d71a5e5146a596c5d62356e5a5e",
                "0xae4d1d0fc4640b63c6ee93c7942106461fb2035f4694b9b6178a5cf5aa94923a",
                "0xd5ec5f248f959407cc9b110ff328752ddd83595304a86e894e5eee51fd9e731f",
                "0xfa22c97f0f4e3029954876eb2de64b621f83fa4f883b5c94b755cde7ec3df6cc",
                "0xb1bfcd1220d2a5e7a47e075c2bed6ec3aed5d40b927980f570d4dc9952cf412f",
                "0x5290109e733eeb86869942bab6710149d2267c830cb95635b9ab187828959887",
                "0x1e074d1961b7318accab00269aa22ca406f3e8112a30eb43c3323fa618f3809e",
                "0x4bf3b20163ec6bfcfaf0ecc0ae2fa8cf7f00d3e44af73fac83e4ee56cd3f7979",
                "0xfc5be3f46628707fbe768e9b82a9c24eb278dda767aa09591b048f1600fe1d5f",
                "0x6789de64eeca68106508168423d9068caa6d994b4bb0366f111fbe4fdeb33074",
                "0x7bcdb4dfc1ec1b8d9b9434406fa6c96c4a12c525f007cd17ef3573a5b8e6e02a",
                "0xe771b5357f00b2abba99971e4bc30467c21c2d87c051decb3d20406c9a462b26",
                "0x4fef38d72c931f4cd9cd478f7afa6e90a03b1b5e4c3fcd212c79e49dbafa99fe",
                "0x2cbe5f3f952759c5b7b83c703a864b23a1258cdd95b9338d5ecc9464f372541a",
                "0x9ba263936803160e04383f02eebdf3ee41bcf8121b42e78a23eb4069863542cd",
                "0xbeafaf164d3782cf8bd483a3e916e116b76503759992f820c0c65a1ec7f41e99",
                "0xfa86ebc0c6ba8cb555208cc777f3ae467b013a4c094f2cfc5fe7b86191279d9b",
                "0xd7efc1445f5f6d2dfccf01caf62abea5b70d4f9e99216fc1fe1a793fcce6ad92",
                "0xc34b546c26fb72b4b50f87b50c66456ea4fca40fc81f7835d5bb2a8c15451102",
                "0x686b845b6943ce828d1bcf4ef6e43f9829284521dc1ec20b62dc9e4e3bfbe33a",
                "0x1ba5fbfa295c82ff0e8d825760056d813f860c950040c6c65f30e46b9c409828",
                "0x37acdb5bf1ef5c1ad932e34f8b96fd9491f4e3bbeda4524b95f2d596b6cc23b9",
                "0xd861a4210be2823d8fd1106df194f38e77dd440568f930217ac6b2fd7f05c370",
                "0x1050e86db22e863aad07a643a4d2f3d22782a250ee3f6c9a75eca1cacf20355a",
                "0x47f6791a64c3e34695ae12da418243071a55e7aea58e0d36bdc71cbbcc259184",
                "0xfe2396021b63644134e00544de91c5d3f78bd22c0680535705167252f4989c80",
                "0x4e24d889374d65f94b7ac2d98766579230312361bd0f956ade2a51be188c88ef",
                "0x38bd51e884266372134d6ef3e9c975be0b7526da158b2a9beac2ecf0751db63c",
                "0x1a04dde1c504c4e070124ddc340320477153c0c49cbfcf2498c25d48f2112d38",
                "0xae74163aa407ed9361f127c48193218d97124e648fdbae773799fa4083710870",
                "0x33f5683b09404c5d76b1933026f16380d358b8c63cf73c8734a121e71d44998b",
                "0x00c7f0789ac26f81c6713a29cbde34b222c8409a013f8edf8c79d44a2e220aaa",
                "0xfbf774d9354b2cbd0181d72c0ed27655360f90f44eab2f9e7456ffd6d8833c3f",
                "0x8488b4df4fa27e379b52c1db42d40d7d0b3ad8ef65c7816d5aea828aa384057c",
                "0x4d86922d4f33497c7943e9fe2ccb28ed8dc7276d43905ea49fadae0eef7ca1b9",
                "0x9ec73359101e0d5a25919342f94ef9ac71287503742a5449b57b0cbf57738480",
                "0xf212d70c71adc44b142bc60012b55a706bd386bcc43564fd16dc96db3063ef35",
                "0xf232464f353ef6df9c599fe85da0d3bb2c7ed9ec1523c39f5ae54a15c4351524",
                "0x697d2d2cf2b9a3612f36ab490464ab6c2893645751c07abec3ad2c4b99217636",
                "0x93ed2d794fc06b940ab6d27f8fa00515ae252df68ad3a542053b3c7a45c96f90",
                "0x3a0f11425350659ad388820321cc0e7c5d512f54dec8b7b684cd2abe4e80bbe1",
                "0x7bdab11f184db7d91e4d1ede74ff905372252824c7aa9716b4927c1d3e9ddf6b",
                "0xe62412f4174df7d3756672a4a975079d21a02c83da96a717c98b2100b9d9f2f3",
                "0xc3e0a531fa5ccaf6259bf3a409a60f594f768bb7fe022e37f406a26e7d9cf39a",
                "0x606b47da5200d5ed58f3b2637ca5177be6e39cd430169a121f54b92b40fe60e4",
                "0x437f15ff92da3b5ad43b1a4e2d0a360c9658265e8ddfb019852069d2aca3fc67",
                "0xadc867cffd29ddf0028f0a37803c56a83e8276bb167fa90cadf60da73ac2790b",
                "0xb5ad9e668b821efb65821d5e21c4aecf4a4f5d841b96dc5db995cc97d76257c8",
                "0x5a81a6c973e178fd3f9496c8a1f3cc474a6bcab4197dfab40ac0c3107a5b3c89",
                "0x2caa1dce52285fbcdcf51480bebc818f1e7bf70fee02a0f06214c09a7a28a196",
                "0x4c39556df98cba6cdc3595c9d92cef9b9e4b4090aec01eb37869111bcfb8d72e",
                "0xf15032450eec09c78053a50f8e0daea023c483ad5b3afbc5e027b4a8c15d7c37",
                "0x6d8816485fa506cf8de8f6500dbcfaf31e50cceb37d7336d773a4c1d0d1d36e9",
                "0x0a509f4d2acd75c9914c83b55fc56eba6e4588f87a1d55a6e0b2e6d86200ff5c",
                "0xfe185246fe31ee4e8fc84a8179c58b6ae1a66b411d7b6a2c78a5446165b50eae",
                "0xa67d33b55be8d35dd3c044f9c26775ff617b246fdda661d8de18e4a00acbe407",
                "0xc8e76771d57161ccc2ddfe74298be21be63ac6902f1046fac635748d43acc973",
                "0x129eeabf53433335e3e23d0ce13e0b9cc5e6ce52f5e08cf089ca1f8afd1f1908",
                "0xb9e1dacfdbcc4c81d3e43cc118a00808fb563dbd6faa52c1045114a963ec2724",
                "0x0d47cadd4a5554f424f259e8a9471d0d53d96da56db2be4b5d670409c24c80f8",
                "0xbf71f596f17c5b115184fafc55accd55b1451a3e26a95340a0d23868a53416f2",
                "0x8f9b5a9600c5c6345b80f627748181cc47dd7777d09f6d698cadf3be7dec7ac2",
                "0x7d0c1722e353d8c0eaafd96ed46e403800fb5cb9c2be385e78f464c378649244",
                "0xf70cf76253f238c3756b05fbcf0df626281b6cc3f90a45224ae59277fe861863",
                "0x5513fccb67a37afc4a3281e0ce950de5bb53954814ebc7cccd42f9fd7e21cabf",
                "0xd2230dd18a2132638257b7c3a2ef144b003e84a4807f64378faa0321bbad5367",
                "0x1cca415bf52b912b97561b984300bbee30cc073b73b21643b92a814c738fdeaf",
                "0x2c1b39dc605653fee98c34808e159bf588fc717839939c5d4546a4c28e72749e",
                "0x86cacfd177cb4a132ae1d852e30b3ce411f47a61d6a745b9a6a33cedd1b63da8",
                "0x581ebfd778154f0a4b630f332caa0f9a0fa7ccb732c56c47b1dadd3e4ca515d1",
                "0xc9462530395c9a128a67f87a8baf9b7bbd186cfdf9a71ad55c0c7c6b502cd1d3",
                "0x0bf6f6ba42a01a83e56250b4f384c57f4dfc7038a342ca2d53a36e897def5498",
                "0x5c1d4552f29de969c456446a91cfb3bbe5cd6ba3504c9d9bc52bb5e60b1865d6",
                "0x41683c9b570ea873b8e63d1a328f45114ac4d4536303a0a1a6fa0e75b1ff551c",
                "0xae7091d4b7a40c423c77f51f2e9abf436fafb96d2b7f1a58855457d0944c527a",
                "0x5c1ff5540120bcf631c0219d7de605cc7030b36aecfffc1d3a99044f21ff8a6d",
                "0x1ddd9994fd8aaf4a95c4a7024f046bfdb37040e18ec6ade81b0a019d72d71fe4",
                "0x771133a50e23d5f71424fbedd76c53ba69d2f65bd46da6fc9fab10e97a2186c8",
                "0xb004b38123b2ecd1fa2fde5e638288179cfd4f397b531c48a55635a1f3d18913",
                "0xa6375cb59a41e1714d11efcf4f5c2fb45ca4410bf066e67dff00a892c4f3bfea",
                "0x64c8ac69973123a35206532df372f4379e1772ca140241be2d0e28f314aff10a",
                "0x30fdf523e5d5c365cdd35a64f617f63294f97b918f936e0549d7e1d32f67b08d",
                "0xe2ccd5e7153a9199e548b7b3e2e1f2c8f6dc45f9ec38b62bc221b81eedd5cc21",
                "0xcfd521f158570cb70805feedaa6cefc337bd4ae40adb0d4e581ef1ec6bd3a738",
                "0xa8eeaeb2f7e4a861dddff2745918e260c86dae71f5d59347d86e879bb72b62cc",
                "0x02ecb7550dae113fbe21bf922daff1d69b905b3135c2382e97db57c7d68bdf35",
                "0xe48aba4f3f6965350616484d87d1e9810bd211dcbb9cd4db07178fbcc6c3ecc3",
                "0x5331cc9a1977dc2195b97be30dc3c048754fab5287eb12e6d048a5062008f811",
                "0x18facaf402bed4d31b09b69f5d5fc708311ebd906f591ae490547a9240d68fba",
                "0x3bde6458531004f1425e91c84176145781c91bb4679087c7ec85c47a89973d85",
                "0x60a62309618e8fb0c8c93fd94e56a34b32e5fdd1c24ed963eda43ac41df5cec5",
                "0x40d9ed4a041601989384bf713f3904c461e62952a8a5371436db16c8f6fb358c",
                "0x3501745563b9a683db893e9891291f13aee8e44301bec38270b8ba9cd39cc628",
                "0x8895759ad3926aa53d161ac05414371e436a05533299ae64ef345758ecd7d4c9",
                "0x524ee4746d7c9460abdde729ec98a98eed603e9e2a05ddf1a9020244a71e02fb",
                "0x9424fac03c7e0027bb4ae73316e7f1bbbbc282c49f5eb6eef3ba82dfc8398a43",
                "0x2579ccf4b3a33a61c9a001119e86f603ddcc0d3ad0a0ab49a319d86ba50b6eda",
                "0x6a29a30f1abb6e89b243a42ffba0da9d5fa0e8c99fdc198e3bf2f80911e2bfb8",
                "0x188bed7449de9710bc413a9c3d113e8950785e3412591bba5a2a0c735c04e50f",
                "0xeb95df2a0570ba85a8226716d14247363409d8df780ffe259c144370616ed5ad",
                "0x86531533a7ee1e7d9c3e1ae428472965320a237ebf095f17c4c85de65688bb01",
                "0xc663db0437f740d02efab521a50775e98a8cac14db132515aba05837d68aeac3",
                "0x5c4f4e2fcbb73465b7742b683cdb94faea199e4e7a7ae62682c2649f364df32f",
                "0xbb04f7bcdd91a103d8051dfa8860d9ef142a07b1fe1f726c5c50bfd601413c13",
                "0xbcb3b14adebc04791a773b806200103bbe216e69012238a63fc8d31da8d71bd9",
                "0x79ac9f93587e5a9b6d9099d02fdaa00aef94f40e54d9adb8624f92f7fa401fb4",
                "0x2f3a2af34e6877d4c31102f07a7c9277997b5f6b7c2e49c1e11164153817f911",
                "0xbf9496dcc0699d0823cf1beb0cce96ea8db04aebf87fb49391b4e6b78b5e9f66",
                "0xe872833f71c0be01ea5e8f400740a7b9a7c71a1cc12aae882274e72da0d038a2",
                "0xbb0e385f93ce1577dc5f01939a47d768745c1020e186981c4bfcf84af5149347",
                "0x0705ffe6f170549772a92b616ed4fdfa9ff1e249211e83cb051bd3413096d2a9",
                "0x509dc32d553554409d5f1f8f21517b094d7072dfd4902fb0cf18f15e01118567",
                "0x023f285dcf1737bd6fddd8c9778353e28536231390c7cb7b452b6a107d7b493e",
                "0x48659d68c785d4f2de553540856c45f9405651dfc388c09c09fa73af7eed4fa3",
                "0x15f6ac1604dc3d0de0e98593cb55528a26b25a19b8491df59109943dde185509",
                "0x22f083e2a99fd3ea33b65a1d234b92b5735452a689796ee3300bde7b0ec376d4",
                "0x8cb15377cc5db95e2d7764e9300ad9e9d3b3fc5811648aaf89f6032cf754c3e7",
                "0x634b1befb2ae2471380b475e1dd6c9f70c138e8b8eeece394508aec2e8bbf5d2",
                "0xf2ed8cca3437e775304cdd7744e699705df8d527f066b95cb85a4d6fe4b3e8cf",
                "0x84dd92ea98ef1de1b2d53dd1759b93885b3b78bbe9266fd65e80eb9e3ba04158",
                "0x5503d852603673c98d84087c49dac843be2b91fa786f6af856d1083e6bdd1a35",
                "0x2ebbfc045cbeaf23681fc17eae4bae43845b633adad561e47ad88dc672b20465",
                "0x223301123e1b5f24dffcd1bafe790861dfd02b1784fd663569196433ac386e2b",
                "0xf2543739ec347da6e70cadc1dd60f90ebd6acd8d48009a8c28f315e1bee17b58",
                "0x0e13cfb2949a69cffea86d1bb4d3abc17a0760ad8129adca28cacc286b9e164f",
                "0xdd1dfa23ed5cc9af6f037e58c65dce7b439c7fed53b59c91edf1cb6823872402",
                "0xb1356f6349e492f2c840b49c79f2eb62cfb47aec9cf9826e7ae4479275b06bb8",
                "0x72c2a70ae442dcb4fadf1468764e4b7f9c2c164a3f47f65c8511a619879dd324",
                "0x98b755c0c1954cccece2b59dd4f9e9f366b3b0ce6df2398998c7e93ec3d8393a",
                "0x1a8a71b84058eae70f4f2d6280b964005a34d78fb58ed0aa0767de9d7d1bb71c",
                "0xc1704f89ad6534ed569d104cf12c08f40e1e181796838ef72e6ca85513527ac1",
                "0x890cf343a5c7e76e1dc5f3b996f8b0f40f45b95ed6022932d9ac746c476f120e",
                "0x35678d33ee0419473979c98d0552993d92a07d32d692c4e7f031a595a69b31e7",
                "0x655c2f100348af45fbd455850263e5c0c11246fab7da424d438db00802c621e5",
                "0xf0ad3c7fe02cb95ef424e7ec766b7da744ddf3b2f2c612e8815ab94a4d38e3b7",
                "0xf8b7ca8a43cb39583c64f459131628a80a36a2d03d5c4e81d51cefeb0617ba02",
                "0x81e298912b35fc17179b61c0bb45ba84095ca0227912fd1a20d675be01a2f469",
                "0xcd372705e368eeb86ad298ed92cefaeee668529fbebde63f0a19af629dc789ec",
                "0xa98c6cd6d198b28cc32132e1f16faf0e3ed36a92f041264b9279a7c6f1c47673",
                "0x3e706ae8066941fc4fc4902d8a7d193fc6269212e81186c7268af65dacba5f84",
                "0xc6dd6674861bc8ff0d342a5a2994a649b091212bd0cf268027b769c081d18742",
                "0xfce86239acf51b656ca20aa5a1f1c3e51e3840ba140759b26d17bd53845ff0bb",
                "0x612271805e26cffdf2b4494be01264960cb45be34c6639eed28f7219d8e14018",
                "0x93cdd04088e940d3d607c27dc7c452c68b2ceb0c890b82f5c22e1461823a5908",
                "0xeb3062b82b14b2b997e6be0a47c96d0fafa03b203c4b4fdd4324d8abfc07eb69",
                "0xb1b4f1040653ff54fefc1c6645fa5443baa20441f8191afc679aa93069fc0fd6"
              ],
              "size": "0x213be",
              "mixHash": "0xaf8463ff5e9d802ca451f1f6128c2e15e9715491d5d8c01546ca4847759ada3f",
              "nonce": "0x0000000000000000",
              "baseFeePerGas": "0x3bb50bfd4"
            },
            "account": {
              "address": "0xb47e3cd837ddf8e4c57f05d70ab865de6e193bbb",
              "balance": "0x10d3603d5cb950e7e8c",
              "codeHash": "0xe2e7a7524a98ce629ee406c15c51a683e4167f0b74ea230566ddece7ae9d6f0b",
              "nonce": "0x1",
              "storageHash": "0x841cb606fbb15fb35132748ca8661dedf4f49c5049d006c36ea5360da82bf0df",
              "accountProof": [
                "0xf90211a0c9e4fc04f7a91b51e5e775b5f3212cfee90816fb0a675e9c7dd11b3a6e23c498a0cf0e901898c98a13edfbffb11576b1a2a6a991347e969af31612a6b462ca63f9a0decf132dd774a452307b6a3f887fcee0b99fbb7e74b527432b15461a19984423a080045acb124a2f19c1a39ff534beef9962f6ca86a954636a6f0d5eedca7383caa0e00069e52dc4b1525b58aca4a9990483c89236e376bb24974aead0613e0fe143a0dc2a16c9795f9740abf2ce3674b2a6d11e9e569ecbaa5425afc9611742ebc51da0e3e62bd4cd6aea039d85f2749d39889cde739a8aed3f109d4d5f84e4b64a5096a03765fc3e56705a90bfa88d6c25211fb68ac35758f6dc3ab97cba4f2052b9f5eda008abf80ff898f778f9061b7d37bb091bb2a4ca2566e38ad8c726e4b6aac71151a03124182279924e97c964b7cfcf17bf94b53fddf5b3e0ea66ee35965cbaa0a4f2a0387fb1aadb93050153228c00c7b8831d01d7b91f1eae28b0e52d5d2adae7c73fa09132f66c6b68c0e1d105c3e28da6a7be7a9579995f34ebe1c02fdb11a2f2bd6ea03f057e46dcac3c25188f66185738349e5072d7d5c39b577d44f62c21e8f7d611a0aa28f3c2e4d518faccd4a5d828c5ea1dd412bf5a9e9581e63d5fdfcd5d4ecf78a062447026ccc1d4a92ebfbd3079ac47c5d79dd932337e14b018d6ba92e3a8eb36a01d52196c05daa8dbf9610d4f0d31fd7f7a73b988bca934813157c5bc456b8d4e80",
                "0xf90211a01e50d27e59fe61d7fc95295acb30b3d6a0fa4b1e965f59b7c2919eaa23631502a07fd9ff9baba117a9b36c752efdc9ea7eb4978f5cc9ec349f5bec298bfe7a238da0f4f3b742871be22951fadfdf5d130a36d22faec89dd244312c4c8bb14e02553aa0a0873624e7121a9505a2714d6d3e1111cf4e4bcd35d63dff9f108925779668fea0a0a9ce55bfdbe7a48238d7b4765dbff584d718f56b91ea6bb48f2dc9bcfee4d8a0711e2d109b3e91550c42fd9822cc0b35668ca6a3f9170d94019b6d659f07fc10a0ca3be9d0e9d0da997506b18779d0018db9500c068a91e17151221d83a170bbfba000d47ad0456345ac6255aed8a1761407105b8370d5e3f67dd110797a4e9b7ddca0eca835d016f610862eb8c2455f9fde7498744ae714d603b173d2d94ac7b68d87a06afd191bfbe65e4c8482176c7bd6d67f04d0b7a84fa34a76254085fb3e1e1c38a0452479f9903d0c02ac370448b6bb03cb810c860a46bbcbba6a2d70e60fa1a3aba05f668c6d251c3274a658d6e524b5b66dbe127ed038fb5fea4228f1de3dd85f12a0c227f93324df1693b6f9a6a61d61ad2c52f08fb06090cdd59843d54d9561bff9a07ad1c53017b78b7356856b1f04340596fabed41b46d1df308546572e5b518393a078c6719c2486bbf246b1dcf5f39d89ebae82d6cb40401ece6ec6811798e8572ca0641173a3e88df7797dca7112b40f05c2a27936a3053067eb41bc0cec7a79c7b780",
                "0xf90211a08ea642a2d35ae6780131074cdcce73cbb13adaa15a34e96d47e88a8e1d527fdaa0fd83441873e59928c1e2dc51af73c55be063d77ce09dd673858efb1b90f24043a01aabdf02f5ccddf297d3403f00342b5697f17cac9babfb8ae96bf74e8c4b684ba063793cb9e94728f7aa252203607ee7761b9b4f419a3fb7dc408d972a07693214a0de2c71503a07d5c40b2df30160513b23709a6166546a03eb345d021a18da1d52a025dbb62ba36d3b285a8dd9a73c47ff10086ea147db64b42da4b6b717466d4dc7a0f790a3765f4951cba71ba391934f6cecb6242717b6d0f75fb7abcc412b3d7245a0dbcb8f7e73e2d2c7c4ff8b85ed9975786f56869d15758acc3d054fb433cce392a00bdd6784b77f29f81744cd657031b653effd5ba0e6e948c51d59567d3ff46ea3a05c90cd70a4cc7b9b9d90af8e919592da20199ab7f6920f36b5be383308ad911ba0198ef1d159eebbc7e3ff64f6b96afb2d444ec75b9e1d4826f4ab234724caba37a0e586a8317bd80cfff44841cc4258c7bdd1f657fd20014e20b398f0771ad523e7a029b677f247bce46a40c5f15c488ab5cdfb574a4403d9bffb27c1e9c70bed6921a0da245c05cf7840099df18ba944ecf57c15bc47357673267986340ab4ab9ef70fa0f2814c5fd35e5cd8d0798b26e4ba9a650e995bb3f01abf02d8eea5664c0007aca0487699b0109d603862086c860630fae06fa3d45079737637f088a2cbb059c31480",
                "0xf90211a0b9ab2564525f02dad287a95130254c079317c76461f2827d8f5389fb826ea6c7a0152b54cd4d775cc045a37a165850700c8842c5cdc7a9b78619856e94cae79a75a01e774ec1938a6c88c40e3da97bf7621aa9ad192056ec718c4604fb66fe33f444a0ec83e48dd3b2bd0866deeb634d261217a13cc3b723720686c09811257a3aea9fa0ff00d9760361a5c8b07ae5d8a5d89f1522bf64b9f222f04f3b565a93ffef0eb6a033d6eadcf568abbb08761cbd11040ecf9c20942e519140f9c97bf8992aecf95aa05725ff93a8628df105e49471a5cd6c0cb03a3bcd5e933f6bf77c8dde8c3c5988a013fc45665bdf530dc5ba871fcb6c9335fbdb95abd36e0eb007acb3041058e56aa0450afa6df06443f30c5dae3bb37d655a3c5c95952998792e08392861ebd43e74a09fd46e66e68ebdda80775433a5aa96196d055732e5fec78ba98494b42398cb75a0450182fda695a66956b9a89010b7e179bd5d38e3744fae13a4b0a7714b08c714a01dd65d7e98fee158adf4bec3c16b39034839fba90a33d52a65787eab5d5d1e9aa0a5500110e38883ff730c44ab9c7efe0ef006a7a29e8b86000bc50c56a3179c2da00f208a0ce0eaf125bfd6f97357b64685ece5af1918f60d4fbe01736701c2eaeda0961888874a11af9bfb571a9324d9216bb74ca407f81023b6a364fd8b61b74e2ba08e4150a038c3ac3772e2db11f4048224cea56f70d5f41b6ca6b4ca5af920e54b80",
                "0xf90211a0c526180429b8eec6eba7de835a7141bed64d6c563e6c30c4ff8bb05f1689b72ea0c4b277acbc788d01b055726b132e05447d7e941c21a37741b92f1847df3ea2c8a072f376d6b9e709d5e49417b7619b3269ee422f6c430e5cf8aaece7daef6eaca1a0834678c04baa71604efc825cdbd096c4f2390593846c3537684718f82d382526a0be154eb528364547b16d8511326c5a857b9ce974ce4e2ececdabb39fd5ed6226a03ee82140f89a97546ac145df76f94375aeb923cd590157c54090f7e0a3a73468a079dd4800d26331f64675c4d934e83b59f2598f46986d52bbf7a5a50d1dd133f2a0f58ce3679ed536daa72b72ddcdd0dedee3cea07e2191c35361f1784e7f3d84afa06746f728649fea30302a09dc7da6992e103904d19615f7aa128c403a3fb4f47aa00975fae22114b798631f9e19ffb9ec4af56b1db2025cae45f4d669ccef6f56f4a06124f9a9d882963fe9e3b1ac2616912f12b8701752090259346d53304792a6daa00176d58e8a4c49e88343cf653ed8ebe3d71c50b501eea58d90fe6a8691ff12f4a088c73544acd0c84c9e67febee417e9d9f0586efa34998a57f3ea9ce93d214479a0afe1e62b4a2b97a30edba7114ebdb996feefabe049dcb954a296b84138ef3d37a09b4fcb84b8eedb0ee10e120861a4fcf3e7f233d8ee2619169e4600c297472a60a02ca2c2c5eb70355a4aed661ce2a26d8e8ddf41fa583202b69151c8043ecf7e5880",
                "0xf90211a0fe802cc20f884ad71df355f4e87178de529967845cc71ea50c1e84f15b8d7fc4a07e6e91ccf3b7d2504b3dde18ae753ffc6e427398082c9d811bc7e097aa6ded87a0832ffab76e6dd0615a2d8894faad599016c8ce9408e9ea082798df7541f59697a00ee266c73038cbba8e23f92fb584b03a0c2435968c3d5aa1e3a650f63b0a8808a0e474e7851ce650ca7b8a23a7f052f85f081550c0d4753aef638a8382e8e3addaa05e40547e9cf3258e69d7371e7109f131928275d7907750b91b689d838ad20692a03acee740dd3779c048b0a0eaad21d279af81479eac2355bc35086bfcbb294e40a0afe8ca8a18ca6fea62cea7ebf9c0ec199bbb0fc928ca558fa57879bd2f9c5896a07f05b1c8946224e64c6b0f08538d9a8aaeb8e028c058329723994e92e57fed01a0fa0a92403bcf280b011b69e11f72c81913a84e2cfa0e73505e3ba85b45c6d198a0b9e1da6493acffd539c1fb56a45681ae4b2bcb8f9c77708e6de8bf244e755abba086ea90b6b4bb2f61d8d2b450321dfb5fb6feaf6b65377ece6687408bf9b2912da07549fa146ce97d39a62845e395d8f45840cea1b7f948deef25ab56e025709cd8a0e1c00f534afba3ecfe1bde3065fc227f5e3a630e621033457239cbd27ee38798a02f20e595c6fa4f443ebf75b38ea4b10ca3666d748931dce34e836fc8b2549faba0deed3e88e193d7bdad0a30a61eed5c0d7562c01c04cb9252da6eff59cbe6972380",
                "0xf8f18080a0b561e85842111223038fd7ef285abf8af348d3f49fdb2a4a6a1976f0a077506980a060e1c6c38ccdc96efaef7dbe161b9e612b4013014e5cc660a0b4cd224024e01f8080a094be361a9ee84da5a699b77e9c999dde4850a31a0dd019130a390613481c36e4a06b9e989cf29f77bc45584c8ad68b1f94c543baee8a1b0f110f528682817b0d9580a0d3e54d8fcf85c438cd773cd432fcaa0d704f6259f9e8a54c938650cd61ee000ca06405ec9caf9e5c8413866d4b68bb46812d2bfdef2c873afc80ab87beb6554de2a03e719b8a8c9fa923e3bfa639c832967256a3532ef23e4a307204510ca8c2cff180808080",
                "0xf8709d3f8c7fab57471a2a41387f9b0d0eab229457c5024bd6cfb72dd7bba2feb850f84e018a010d3603d5cb950e7e8ca0841cb606fbb15fb35132748ca8661dedf4f49c5049d006c36ea5360da82bf0dfa0e2e7a7524a98ce629ee406c15c51a683e4167f0b74ea230566ddece7ae9d6f0b"
              ],
              "storageProof": [
                {
                  "key": "0x13da86008ba1c6922daee3e07db95305ef49ebced9f5467a0b8613fcc6b343e3",
                  "proof": [
                    "0xf90211a060252d195860939219df3716cb285660bd749cb2e71aced70161504f702ff38ca06210c127c590fb12768f04e8793f5c9b35cf8db2cf41f668bb5eacef3c30b2c4a0789aca4da06a78f6423c9bcff43cd048b304e5b6fc3609016294b52d5b6676aca08af8197ed18722d092f045be108791a65aad5976d573f840dd04ac7104335658a02e752a1f660263001144a2cceddf9c7f994de89d83df2f42ebf6f8b8027113b9a06745c84e3a033107857a844d60617a628effac29c82f18f323087e3e18bbe152a0952508605c8c0efa69c2d9eb29712fbd3fffa1ac0321432ccc5feade9cf97627a026960a36654a084da4a53dae9fd006fce46e77960a23d86736fc9b5aca4b925ea0363ec9f1730e0634e4a16fa077651247352dd16bd73db6d37bfd87892613d18ba0f7be2c53d3bb24c89a4bc51afc628e7f405ddf849e46d267ad576039ca326645a075f5a66afe20ddf0e1b525c0b15ee2c61fa1f7eb311859b3fd098d4b2978de27a04656cd4e1bab1c907d32f3884a0b34220ea80774dac5a29fb0616369cf274c11a03e3af59105ef1a3e8ae7d61b45f913766324d64e3bf577baf442cbf6c555d8b2a035e06cd495849b7fb2c6a175a30e22f78a4475f28b0de4025a9c49836f737082a061ac9b7f5825e5ac5f96cb366df00b227cc62a7bd8460e1d3ab9567a693c2b9fa033d577f9add045a435f41b814d70ca0b0d2735e74dcf8b1d4b5f04a4923e45b480",
                    "0xf90211a03fb758b635db89b71f610d2657590bfb2f1560f925962df48532f79be5283d76a0142444f97cc3aa7533521706788b11339354b4cbe352c11d672db90b0d6fdcb3a022239d6bf9b90fcf67c2b2875ad98b4e6e0dbc6601c41378fbc229db96d8712da00bb86c97bdc035a8341410aa4898e55e6d98af365702d421f768a6443ddd1386a0d1807dbc8b1974def62740e95fd80d6e80d102c90dea31aa1fedb41b22a23d87a0f0b412cbc3bc1024fab380871bf6720e44a62f91510fdc41ed8b1c52e33bd991a0bdb50097ebf1deff39e72fc6e035461cd946005e9aa68505c281f0a8231286cda089ff51406ba16f88e511f7544796fd0a1d7b4fdbc10f7bce5a1a3def49f7ad8aa07dcb3a87c0edcfbd974eb696bee69ea6b5ac8d93b9bd2c2c2a9f0249f50fb3aca056a34da1bc930167bdf97e9f1328a8d217deb91ee3f6e66cbce114b93eebf951a0215cd46425f3ba378a0e579caf4460813fb9a88f7f16455ff8fa8f1845ec0445a00c93cf7be4f553c0b60288cd89117beacc144ee70dc0a39780ad1120b04ac67ba001f71899364477b33fd0126f43656cc9b53612438f7a26f039e5b102f97ee018a05b63bef9d1924041b93febe38e68238a345b70940b719a590956f1cd31c9fcbba0e405622bf9a63735e72b40d0960581d8f1cc0a5bd9efd5d1282ed232c515d322a03c7add9b013ff8f3654a95b8efa42ed098928e60ecf9eebb1f6159d458f9160e80",
                    "0xf90211a0587ebbf59a619da308c15525bd0ddbbec75f42261bb502604c81f48113e9b6eba0ead73847aa84dec1093fa40a5f78a7a8c39cd45c32041a9c01c7a8cb030f3034a00c210c8e180468799bc26608c637817eadd4168445039879ec418da237436604a001cafc8a7f4728bf2afeb4f5f841bbaff92943b8a631d70228bb2021a2255862a070d6f25250d485c8a7250e810d21cc20a252efcf60a676e3ddd1ca3dafb123b7a065a2ff940acd03d2c93101ec97236c4fa3a3a593be9c4882f49215f4163d3c98a09c2b4bb6f6c6d4ad30c6e959ad224f2fb1cb7792ffe84d1f8b3e160c26911953a0f71584c68ee3cbae0107923ff0cf46ff461c451c526a746d93bee9e325f4ba67a017ba5ae5913e21ab8257e97cde204a69d5aa09d9f300cfcf3cabd28c2906df51a0106deacc39677eddd310a000b078ff1bcf8b6a08a28de4655dfeaf2496d7f6a0a0180a1d1662e98f83aeac6397fa3cf049a770a9560bcdab9e92c74d852c3403b3a0bc5704dfaef66221b061f9a040c8bec2aec42d12100e45fab425affeffdd4a18a06b3f302cf3c83c7f7cff2d6361734abe3ab8bd0feac33e0bc8297c05a8dcc30aa041fd229078ab04fa2e8a8a667256914c98a983ef8d999d00a5a3112ddc7d140aa02de44e11c8094583d7c00bd2061fab8531c8d3c8686905e1a14824e817898b4ba0827638d5fd5697543ce110e8899e27e09f5ae28cc5e447a7351f75a84b44f98280",
                    "0xf8f1a08b59042d7dc9e951b13b7f2e64195d539a4956260c422ac3a7bbfc1be7406b38808080a0496aa1a457432e801b32a427e192e8425f62bf6fbb174fc68037478cc4c89cfc8080a0545f004f920066bd719b50e46938c9fcd8fd86f658da6409f2e12156574ae977a0e46be32f90c4c8de23a8b29f6057cdd964415dc63a5b52c01d87ffa84cb18f00a012eadacd1fc4cbdd537c33bbab21f3acb481784598410421ad36c43ba19dfbbfa0397cdb9a0ba2086382937ddd86db84961dc4e86ce2be3e103f24276fe4302509a094f91eae8184b46734e2d0937d9bad9222039a6cce993b95debd03622ee36c168080808080",
                    "0xf69f200b972c470f18f842c35c712f9b9beaac1e8e838c1ba491d0d478900e81ca9594e08c32737c021c7d05d116b00a68a02f2d144ac0"
                  ],
                  "value": "0xe08c32737c021c7d05d116b00a68a02f2d144ac0"
                }
              ]
            }  
          }
        "#;
        use crate::providers::saved_block_storage_input_fromstr;

        let inputs = saved_block_storage_input_fromstr(s);
        let network = Network::Mainnet;

        Self { inputs, network, _marker: PhantomData }
    }
}

impl<F: Field> Circuit<F> for EthBlockStorageCircuit<F> {
    type Config = EthConfig<F>;
    type FloorPlanner = SimpleFloorPlanner;

    fn without_witnesses(&self) -> Self {
        self.clone()
    }

    fn configure(meta: &mut ConstraintSystem<F>) -> Self::Config {

        let params = EthConfigParams{
            degree: 16,
            num_rlc_columns: 2,
            num_range_advice: [20,15].into(),
            num_lookup_advice: [1,1].into(),
            num_fixed: 1,
            unusable_rows: 79,
            keccak_rows_per_round: 16
        };

        //let params = EthConfigParams::get_storage();
        EthConfig::configure(meta, params, 0)
    }

    fn synthesize(
        &self,
        config: Self::Config,
        mut layouter: impl Layouter<F>,
    ) -> Result<(), Error> {
        #[cfg(feature = "display")]
        let witness_gen = start_timer!(|| "synthesize");

        let gamma = layouter.get_challenge(config.rlc().gamma);
        config.range().load_lookup_table(&mut layouter).expect("load range lookup table");
        config.keccak().load_aux_tables(&mut layouter).expect("load keccak lookup tables");

        let mut first_pass = SKIP_FIRST_PASS;
        let mut instance = vec![];
        layouter
            .assign_region(
                || "eth_getProof verify from blockHash",
                |region| {
                    if first_pass {
                        first_pass = false;
                        return Ok(());
                    }
                    let mut chip = EthChip::new(config.clone(), gamma);
                    let mut aux = Context::new(
                        region,
                        ContextParams {
                            max_rows: chip.gate().max_rows,
                            num_context_ids: 2,
                            fixed_columns: chip.gate().constants.clone(),
                        },
                    );
                    let ctx = &mut aux;

                    // ================= FIRST PHASE ================
                    let input = self.inputs.assign(ctx, chip.gate());
                    let witness =
                        chip.parse_eip1186_proofs_from_block_phase0(ctx, input, self.network);
                    chip.assign_phase0(ctx);
                    ctx.next_phase();

                    // ================= SECOND PHASE ================
                    chip.get_challenge(ctx);
                    chip.keccak_assign_phase1(ctx);

                    let trace = chip.parse_eip1186_proofs_from_block_phase1(ctx, witness);
                    let EIP1186ResponseDigest { block_hash, block_number, address, slots_values } =
                        trace.digest;
                    chip.range().finalize(ctx);

                    instance.extend(
                        block_hash
                            .iter()
                            .chain([block_number, address].iter())
                            .chain(
                                slots_values
                                    .iter()
                                    .flat_map(|(slot, value)| slot.iter().chain(value.iter())),
                            )
                            .map(|acell| acell.cell().clone()),
                    );

                    #[cfg(feature = "display")]
                    ctx.print_stats(&["Range", "RLC"]);
                    Ok(())
                },
            )
            .unwrap();
        for (i, cell) in instance.into_iter().enumerate() {
            layouter.constrain_instance(cell, config.instance, i);
        }
        #[cfg(feature = "display")]
        end_timer!(witness_gen);
        Ok(())
    }
}

impl<F: Field> CircuitExt<F> for EthBlockStorageCircuit<F> {
    fn num_instance(&self) -> Vec<usize> {
        vec![4 + 4 * self.inputs.storage.storage_pfs.len()]
    }

    fn instances(&self) -> Vec<Vec<F>> {
        vec![self.instance()]
    }
}
