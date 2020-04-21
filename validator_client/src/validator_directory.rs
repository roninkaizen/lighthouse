use bls::get_withdrawal_credentials;
use deposit_contract::encode_eth1_tx_data;
use hex;
use slashing_protection::{
    signed_attestation::SignedAttestation,
    signed_block::SignedBlock,
    validator_history::{SlashingProtection as SlashingProtectionTrait, ValidatorHistory},
};
use ssz::{Decode, Encode};
use ssz_derive::{Decode, Encode};
use std::fs;
use std::fs::File;
use std::io::prelude::*;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use types::{
    test_utils::generate_deterministic_keypair, ChainSpec, DepositData, Hash256, Keypair,
    PublicKey, SecretKey, Signature,
};

const VOTING_KEY_PREFIX: &str = "voting";
const WITHDRAWAL_KEY_PREFIX: &str = "withdrawal";
const ETH1_DEPOSIT_DATA_FILE: &str = "eth1_deposit_data.rlp";
pub const ATTESTER_SLASHING_DB: &str = "attester_slashing_protection.sqlite";
pub const BLOCK_PRODUCER_SLASHING_DB: &str = "block_producer_slashing_protection.sqlite";

/// Returns the filename of a keypair file.
fn keypair_file(prefix: &str) -> String {
    format!("{}_keypair", prefix)
}

/// Returns the name of the folder to be generated for a validator with the given voting key.
fn dir_name(voting_pubkey: &PublicKey) -> String {
    format!("0x{}", hex::encode(voting_pubkey.as_ssz_bytes()))
}

/// Represents the files/objects for each dedicated lighthouse validator directory.
///
/// Generally lives in `~/.lighthouse/validators/`.
#[derive(Debug, PartialEq, Clone)]
pub struct ValidatorDirectory {
    pub directory: PathBuf,
    pub voting_keypair: Option<Keypair>,
    pub withdrawal_keypair: Option<Keypair>,
    pub deposit_data: Option<Vec<u8>>,
    pub attestation_slashing_protection: Option<PathBuf>,
    pub block_slashing_protection: Option<PathBuf>,
    pub slots_per_epoch: Option<u64>,
}

impl ValidatorDirectory {
    /// Attempts to load a validator from the given directory, requiring only components necessary
    /// for signing messages.
    pub fn load_for_signing(directory: PathBuf, slots_per_epoch: u64) -> Result<Self, String> {
        if !directory.exists() {
            return Err(format!(
                "Validator directory does not exist: {:?}",
                directory
            ));
        }

        let attestation_slashing_protection = directory.join(ATTESTER_SLASHING_DB);
        let block_slashing_protection = directory.join(BLOCK_PRODUCER_SLASHING_DB);

        if !(attestation_slashing_protection.exists() && block_slashing_protection.exists()) {
            return Err(format!(
                "Unable to find slashing protection in {:?}",
                directory
            ));
        }
        // Loading the block proposer db to retrieve its slots_per_epoch data
        let block_history: ValidatorHistory<SignedBlock> =
            ValidatorHistory::open(&block_slashing_protection, Some(slots_per_epoch))
                .map_err(|e| e.to_string())?;
        let slots_per_epoch = block_history.slots_per_epoch().map_err(|e| e.to_string())?;

        Ok(Self {
            voting_keypair: Some(
                load_keypair(directory.clone(), VOTING_KEY_PREFIX)
                    .map_err(|e| format!("Unable to get voting keypair: {}", e))?,
            ),
            withdrawal_keypair: load_keypair(directory.clone(), WITHDRAWAL_KEY_PREFIX).ok(),
            deposit_data: load_eth1_deposit_data(directory.clone()).ok(),
            directory,
            attestation_slashing_protection: Some(attestation_slashing_protection),
            block_slashing_protection: Some(block_slashing_protection),
            slots_per_epoch: Some(slots_per_epoch),
        })
    }
}

/// Load a `Keypair` from a file.
fn load_keypair(base_path: PathBuf, file_prefix: &str) -> Result<Keypair, String> {
    let path = base_path.join(keypair_file(file_prefix));

    if !path.exists() {
        return Err(format!("Keypair file does not exist: {:?}", path));
    }

    let mut bytes = vec![];

    File::open(&path)
        .map_err(|e| format!("Unable to open keypair file: {}", e))?
        .read_to_end(&mut bytes)
        .map_err(|e| format!("Unable to read keypair file: {}", e))?;

    SszEncodableKeypair::from_ssz_bytes(&bytes)
        .map(Into::into)
        .map_err(|e| format!("Unable to decode keypair: {:?}", e))
}

/// Load eth1_deposit_data from file.
fn load_eth1_deposit_data(base_path: PathBuf) -> Result<Vec<u8>, String> {
    let path = base_path.join(ETH1_DEPOSIT_DATA_FILE);

    if !path.exists() {
        return Err(format!("Eth1 deposit data file does not exist: {:?}", path));
    }

    let mut bytes = vec![];

    File::open(&path)
        .map_err(|e| format!("Unable to open eth1 deposit data file: {}", e))?
        .read_to_end(&mut bytes)
        .map_err(|e| format!("Unable to read eth1 deposit data file: {}", e))?;

    let string = String::from_utf8_lossy(&bytes);
    if string.starts_with("0x") {
        hex::decode(&string[2..])
            .map_err(|e| format!("Unable to decode eth1 data file as hex: {}", e))
    } else {
        Err(format!("String did not start with 0x: {}", string))
    }
}

/// A helper struct to allow SSZ enc/dec for a `Keypair`.
#[derive(Encode, Decode)]
struct SszEncodableKeypair {
    pk: PublicKey,
    sk: SecretKey,
}

impl Into<Keypair> for SszEncodableKeypair {
    fn into(self) -> Keypair {
        Keypair {
            sk: self.sk,
            pk: self.pk,
        }
    }
}

impl From<Keypair> for SszEncodableKeypair {
    fn from(kp: Keypair) -> Self {
        Self {
            sk: kp.sk,
            pk: kp.pk,
        }
    }
}

/// Builds a `ValidatorDirectory`, both in-memory and on-disk.
#[derive(Default)]
pub struct ValidatorDirectoryBuilder {
    directory: Option<PathBuf>,
    voting_keypair: Option<Keypair>,
    withdrawal_keypair: Option<Keypair>,
    amount: Option<u64>,
    deposit_data: Option<Vec<u8>>,
    attestation_slashing_protection: Option<PathBuf>,
    block_slashing_protection: Option<PathBuf>,
    spec: Option<ChainSpec>,
    slots_per_epoch: Option<u64>,
}

impl ValidatorDirectoryBuilder {
    /// Set the specification for this validator.
    pub fn spec(mut self, spec: ChainSpec) -> Self {
        self.spec = Some(spec);
        self
    }

    /// Use the `MAX_EFFECTIVE_BALANCE` as this validator's deposit.
    pub fn full_deposit_amount(mut self) -> Result<Self, String> {
        let spec = self
            .spec
            .as_ref()
            .ok_or_else(|| "full_deposit_amount requires a spec")?;
        self.amount = Some(spec.max_effective_balance);
        Ok(self)
    }

    /// Use a validator deposit of `gwei`.
    pub fn custom_deposit_amount(mut self, gwei: u64) -> Self {
        self.amount = Some(gwei);
        self
    }

    /// Generate keypairs using `Keypair::random()`.
    pub fn thread_random_keypairs(mut self) -> Self {
        self.voting_keypair = Some(Keypair::random());
        self.withdrawal_keypair = Some(Keypair::random());
        self
    }

    /// Sets the slots_per_epoch
    pub fn slots_per_epoch(mut self, slots_per_epoch: u64) -> Self {
        self.slots_per_epoch = Some(slots_per_epoch);
        self
    }

    /// Generate insecure, deterministic keypairs.
    ///
    ///
    /// ## Warning
    /// Only for use in testing. Do not store value in these keys.
    pub fn insecure_keypairs(mut self, index: usize) -> Self {
        let keypair = generate_deterministic_keypair(index);
        self.voting_keypair = Some(keypair.clone());
        self.withdrawal_keypair = Some(keypair);
        self
    }

    /// Creates a validator directory in the given `base_path` (e.g., `~/.lighthouse/validators/`).
    pub fn create_directory(mut self, base_path: PathBuf) -> Result<Self, String> {
        let voting_keypair = self
            .voting_keypair
            .as_ref()
            .ok_or_else(|| "directory requires a voting_keypair")?;

        let directory = base_path.join(dir_name(&voting_keypair.pk));

        if directory.exists() {
            return Err(format!(
                "Validator directory already exists: {:?}",
                directory
            ));
        }

        fs::create_dir_all(&directory)
            .map_err(|e| format!("Unable to create validator directory: {}", e))?;

        self.directory = Some(directory);

        Ok(self)
    }

    /// Write the validators keypairs to disk.
    pub fn write_keypair_files(self) -> Result<Self, String> {
        let voting_keypair = self
            .voting_keypair
            .clone()
            .ok_or_else(|| "write_keypair_files requires a voting_keypair")?;
        let withdrawal_keypair = self
            .withdrawal_keypair
            .clone()
            .ok_or_else(|| "write_keypair_files requires a withdrawal_keypair")?;

        self.save_keypair(voting_keypair, VOTING_KEY_PREFIX)?;
        self.save_keypair(withdrawal_keypair, WITHDRAWAL_KEY_PREFIX)?;
        Ok(self)
    }

    fn save_keypair(&self, keypair: Keypair, file_prefix: &str) -> Result<(), String> {
        let path = self
            .directory
            .as_ref()
            .map(|directory| directory.join(keypair_file(file_prefix)))
            .ok_or_else(|| "save_keypair requires a directory")?;

        if path.exists() {
            return Err(format!("Keypair file already exists at: {:?}", path));
        }

        let mut file = File::create(&path).map_err(|e| format!("Unable to create file: {}", e))?;

        // Ensure file has correct permissions.
        let mut perm = file
            .metadata()
            .map_err(|e| format!("Unable to get file metadata: {}", e))?
            .permissions();
        perm.set_mode((libc::S_IWUSR | libc::S_IRUSR) as u32);
        file.set_permissions(perm)
            .map_err(|e| format!("Unable to set file permissions: {}", e))?;

        file.write_all(&SszEncodableKeypair::from(keypair).as_ssz_bytes())
            .map_err(|e| format!("Unable to write keypair to file: {}", e))?;

        Ok(())
    }

    /// Write the validators eth1 deposit transaction data to file. This can be used to submit a
    /// validator deposit.
    pub fn write_eth1_data_file(mut self) -> Result<Self, String> {
        let voting_keypair = self
            .voting_keypair
            .as_ref()
            .ok_or_else(|| "write_eth1_data_file requires a voting_keypair")?;
        let withdrawal_keypair = self
            .withdrawal_keypair
            .as_ref()
            .ok_or_else(|| "write_eth1_data_file requires a withdrawal_keypair")?;
        let amount = self
            .amount
            .ok_or_else(|| "write_eth1_data_file requires an amount")?;
        let spec = self.spec.as_ref().ok_or_else(|| "build requires a spec")?;
        let path = self
            .directory
            .as_ref()
            .map(|directory| directory.join(ETH1_DEPOSIT_DATA_FILE))
            .ok_or_else(|| "write_eth1_data_file requires a directory")?;

        let deposit_data = {
            let withdrawal_credentials = Hash256::from_slice(&get_withdrawal_credentials(
                &withdrawal_keypair.pk,
                spec.bls_withdrawal_prefix_byte,
            ));

            let mut deposit_data = DepositData {
                pubkey: voting_keypair.pk.clone().into(),
                withdrawal_credentials,
                amount,
                signature: Signature::empty_signature().into(),
            };

            deposit_data.signature = deposit_data.create_signature(&voting_keypair.sk, &spec);

            encode_eth1_tx_data(&deposit_data)
                .map_err(|e| format!("Unable to encode eth1 deposit tx data: {:?}", e))?
        };

        if path.exists() {
            return Err(format!("Eth1 data file already exists at: {:?}", path));
        }

        File::create(&path)
            .map_err(|e| format!("Unable to create file: {}", e))?
            .write_all(&format!("0x{}", hex::encode(&deposit_data)).as_bytes())
            .map_err(|e| format!("Unable to write eth1 data file: {}", e))?;

        self.deposit_data = Some(deposit_data);

        Ok(self)
    }

    /// Creates two sqlite slashing protection databases (blocks and attestations) and stores the
    /// paths.
    pub fn create_sqlite_slashing_dbs(mut self) -> Result<Self, String> {
        let path = self
            .directory
            .as_ref()
            .ok_or_else(|| "create_sqlite_slashing_dbs requires a directory")?;

        let attestation_path = path.join(ATTESTER_SLASHING_DB);
        let block_path = path.join(BLOCK_PRODUCER_SLASHING_DB);
        let slots_per_epoch = self.slots_per_epoch;

        let _: ValidatorHistory<SignedAttestation> = ValidatorHistory::new(&attestation_path, None)
            .map_err(|e| format!("Unable to create {:?}: {:?}", attestation_path, e))?;

        let _: ValidatorHistory<SignedBlock> =
            ValidatorHistory::new(&path.join(&block_path), slots_per_epoch)
                .map_err(|e| format!("Unable to create {:?}: {:?}", block_path, e))?;

        self.attestation_slashing_protection = Some(attestation_path);
        self.block_slashing_protection = Some(block_path);

        Ok(self)
    }

    /// Consumes self, returning a `ValidatorDirectory` on success.
    pub fn build(self) -> Result<ValidatorDirectory, String> {
        let directory = self.directory.ok_or_else(|| "build requires a directory")?;

        Ok(ValidatorDirectory {
            directory,
            voting_keypair: self.voting_keypair,
            withdrawal_keypair: self.withdrawal_keypair,
            deposit_data: self.deposit_data,
            attestation_slashing_protection: self.attestation_slashing_protection,
            block_slashing_protection: self.block_slashing_protection,
            slots_per_epoch: self.slots_per_epoch,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempdir::TempDir;
    use types::{EthSpec, MinimalEthSpec};

    type E = MinimalEthSpec;

    #[test]
    fn random_keypairs_round_trip() {
        let spec = E::default_spec();
        let temp_dir = TempDir::new("acc_manager").expect("should create test dir");

        let created_dir = ValidatorDirectoryBuilder::default()
            .spec(spec)
            .slots_per_epoch(E::slots_per_epoch())
            .full_deposit_amount()
            .expect("should set full deposit amount")
            .thread_random_keypairs()
            .create_directory(temp_dir.path().into())
            .expect("should create directory")
            .write_keypair_files()
            .expect("should write keypair files")
            .write_eth1_data_file()
            .expect("should write eth1 data file")
            .create_sqlite_slashing_dbs()
            .expect("should create slashing dbs")
            .build()
            .expect("should build dir");

        let loaded_dir = ValidatorDirectory::load_for_signing(
            created_dir.directory.clone(),
            E::slots_per_epoch(),
        )
        .expect("should load directory");

        assert_eq!(
            created_dir, loaded_dir,
            "the directory created should match the one loaded"
        );
    }

    #[test]
    fn deterministic_keypairs_round_trip() {
        let spec = E::default_spec();
        let temp_dir = TempDir::new("acc_manager").expect("should create test dir");
        let index = 42;

        let created_dir = ValidatorDirectoryBuilder::default()
            .spec(spec)
            .slots_per_epoch(E::slots_per_epoch())
            .full_deposit_amount()
            .expect("should set full deposit amount")
            .insecure_keypairs(index)
            .create_directory(temp_dir.path().into())
            .expect("should create directory")
            .write_keypair_files()
            .expect("should write keypair files")
            .write_eth1_data_file()
            .expect("should write eth1 data file")
            .create_sqlite_slashing_dbs()
            .expect("should create slashing dbs")
            .build()
            .expect("should build dir");

        assert!(
            created_dir.directory.exists(),
            "should have created directory"
        );

        let mut parent = created_dir.directory.clone();
        parent.pop();
        assert_eq!(
            parent,
            PathBuf::from(temp_dir.path()),
            "should have created directory ontop of base dir"
        );

        let expected_keypair = generate_deterministic_keypair(index);
        assert_eq!(
            created_dir.voting_keypair,
            Some(expected_keypair.clone()),
            "voting keypair should be as expected"
        );
        assert_eq!(
            created_dir.withdrawal_keypair,
            Some(expected_keypair),
            "withdrawal keypair should be as expected"
        );
        assert!(
            created_dir.attestation_slashing_protection.is_some(),
            "attestation slashing db file should exist"
        );
        assert!(
            created_dir.block_slashing_protection.is_some(),
            "block slashing db file should exist"
        );
        assert!(
            !created_dir
                .deposit_data
                .clone()
                .expect("should have data")
                .is_empty(),
            "should have some deposit data"
        );

        let loaded_dir = ValidatorDirectory::load_for_signing(
            created_dir.directory.clone(),
            E::slots_per_epoch(),
        )
        .expect("should load directory");

        assert_eq!(
            created_dir, loaded_dir,
            "the directory created should match the one loaded"
        );
    }
}
