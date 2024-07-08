// Copyright 2018-2024 the Deno authors. All rights reserved. MIT license.

use std::borrow::Cow;
use std::cell::RefCell;

use base64::Engine;
use deno_core::error::generic_error;
use deno_core::error::type_error;
use deno_core::error::AnyError;
use deno_core::op2;
use deno_core::serde_v8::BigInt as V8BigInt;
use deno_core::unsync::spawn_blocking;
use deno_core::GarbageCollected;
use ed25519_dalek::pkcs8::BitStringRef;
use num_bigint::BigInt;
use num_traits::FromPrimitive as _;
use once_cell::sync::Lazy;
use pkcs8::DecodePrivateKey as _;
use pkcs8::Document;
use pkcs8::EncodePrivateKey as _;
use pkcs8::EncryptedPrivateKeyInfo;
use pkcs8::PrivateKeyInfo;
use pkcs8::SecretDocument;
use rand::thread_rng;
use rand::RngCore as _;
use rsa::pkcs1::DecodeRsaPrivateKey as _;
use rsa::pkcs1::DecodeRsaPublicKey;
use rsa::pkcs1::EncodeRsaPrivateKey as _;
use rsa::pkcs1::EncodeRsaPublicKey;
use rsa::traits::PublicKeyParts;
use rsa::RsaPrivateKey;
use rsa::RsaPublicKey;
use sec1::der::Writer as _;
use sec1::pem::PemLabel as _;
use sec1::DecodeEcPrivateKey as _;
use sec1::LineEnding;
use spki::der::asn1;
use spki::der::Decode as _;
use spki::der::Encode as _;
use spki::der::PemWriter;
use spki::der::Reader as _;
use spki::DecodePublicKey as _;
use spki::EncodePublicKey as _;
use spki::SubjectPublicKeyInfoRef;

#[derive(Clone)]
pub enum KeyObjectHandle {
  AsymmetricPrivateKey(AsymmetricPrivateKey),
  AsymmetricPublicKey(AsymmetricPublicKey),
  SecretKey(Box<[u8]>),
}

impl GarbageCollected for KeyObjectHandle {}

#[derive(Clone)]
pub enum AsymmetricPrivateKey {
  Rsa(RsaPrivateKey),
  RsaPss(RsaPssPrivateKey),
  Dsa(dsa::SigningKey),
  Ec(EcPrivateKey),
  X25519(x25519_dalek::StaticSecret),
  Ed25519(ed25519_dalek::SigningKey),
  #[allow(unused)]
  Dh(Box<[u8]>),
}

#[derive(Clone)]
pub struct RsaPssPrivateKey {
  pub key: RsaPrivateKey,
  pub hash_algorithm: RsaPssHashAlgorithm,
  pub salt_length: u32,
}

#[derive(Clone, Copy)]
pub enum RsaPssHashAlgorithm {
  Sha1,
  Sha256,
  Sha384,
  Sha512,
}

#[derive(Clone)]
pub enum EcPrivateKey {
  P224(p224::SecretKey),
  P256(p256::SecretKey),
  P384(p384::SecretKey),
}

#[derive(Clone)]
pub enum AsymmetricPublicKey {
  Rsa(rsa::RsaPublicKey),
  RsaPss(RsaPssPublicKey),
  Dsa(dsa::VerifyingKey),
  Ec(EcPublicKey),
  #[allow(unused)]
  X25519(x25519_dalek::PublicKey),
  Ed25519(ed25519_dalek::VerifyingKey),
  #[allow(unused)]
  Dh(Box<[u8]>),
}

#[derive(Clone)]
pub struct RsaPssPublicKey {
  pub key: rsa::RsaPublicKey,
  pub hash_algorithm: RsaPssHashAlgorithm,
  pub salt_length: u32,
}

#[derive(Clone)]
pub enum EcPublicKey {
  P224(p224::PublicKey),
  P256(p256::PublicKey),
  P384(p384::PublicKey),
}

impl KeyObjectHandle {
  /// Returns the private key if the handle is an asymmetric private key.
  pub fn as_private_key(&self) -> Option<&AsymmetricPrivateKey> {
    match self {
      KeyObjectHandle::AsymmetricPrivateKey(key) => Some(key),
      _ => None,
    }
  }

  /// Returns the public key if the handle is an asymmetric public key. If it is
  /// a private key, it derives the public key from it and returns that.
  pub fn as_public_key(&self) -> Option<Cow<'_, AsymmetricPublicKey>> {
    match self {
      KeyObjectHandle::AsymmetricPrivateKey(key) => {
        Some(Cow::Owned(key.to_public_key()))
      }
      KeyObjectHandle::AsymmetricPublicKey(key) => Some(Cow::Borrowed(key)),
      _ => None,
    }
  }

  /// Returns the secret key if the handle is a secret key.
  pub fn as_secret_key(&self) -> Option<&[u8]> {
    match self {
      KeyObjectHandle::SecretKey(key) => Some(key),
      _ => None,
    }
  }
}

impl AsymmetricPrivateKey {
  /// Derives the public key from the private key.
  pub fn to_public_key(&self) -> AsymmetricPublicKey {
    match self {
      AsymmetricPrivateKey::Rsa(key) => {
        AsymmetricPublicKey::Rsa(key.to_public_key())
      }
      AsymmetricPrivateKey::RsaPss(key) => {
        AsymmetricPublicKey::RsaPss(key.to_public_key())
      }
      AsymmetricPrivateKey::Dsa(key) => {
        AsymmetricPublicKey::Dsa(key.verifying_key().clone())
      }
      AsymmetricPrivateKey::Ec(key) => {
        AsymmetricPublicKey::Ec(key.to_public_key())
      }
      AsymmetricPrivateKey::X25519(key) => {
        AsymmetricPublicKey::X25519(x25519_dalek::PublicKey::from(key))
      }
      AsymmetricPrivateKey::Ed25519(key) => {
        AsymmetricPublicKey::Ed25519(key.verifying_key())
      }
      AsymmetricPrivateKey::Dh(_) => {
        panic!("cannot derive public key from DH private key")
      }
    }
  }
}

impl RsaPssPrivateKey {
  /// Derives the public key from the private key.
  pub fn to_public_key(&self) -> RsaPssPublicKey {
    RsaPssPublicKey {
      key: self.key.to_public_key(),
      hash_algorithm: self.hash_algorithm,
      salt_length: self.salt_length,
    }
  }
}

impl EcPrivateKey {
  /// Derives the public key from the private key.
  pub fn to_public_key(&self) -> EcPublicKey {
    match self {
      EcPrivateKey::P224(key) => EcPublicKey::P224(key.public_key()),
      EcPrivateKey::P256(key) => EcPublicKey::P256(key.public_key()),
      EcPrivateKey::P384(key) => EcPublicKey::P384(key.public_key()),
    }
  }
}

// https://oidref.com/
const ID_SHA1_OID: rsa::pkcs8::ObjectIdentifier =
  rsa::pkcs8::ObjectIdentifier::new_unwrap("1.3.14.3.2.26");
const ID_SHA256_OID: rsa::pkcs8::ObjectIdentifier =
  rsa::pkcs8::ObjectIdentifier::new_unwrap("2.16.840.1.101.3.4.2.1");
const ID_SHA384_OID: rsa::pkcs8::ObjectIdentifier =
  rsa::pkcs8::ObjectIdentifier::new_unwrap("2.16.840.1.101.3.4.2.2");
const ID_SHA512_OID: rsa::pkcs8::ObjectIdentifier =
  rsa::pkcs8::ObjectIdentifier::new_unwrap("2.16.840.1.101.3.4.2.3");
const ID_MFG1: rsa::pkcs8::ObjectIdentifier =
  rsa::pkcs8::ObjectIdentifier::new_unwrap("1.2.840.113549.1.1.8");
pub const ID_SECP224R1_OID: const_oid::ObjectIdentifier =
  const_oid::ObjectIdentifier::new_unwrap("1.3.132.0.33");
pub const ID_SECP256R1_OID: const_oid::ObjectIdentifier =
  const_oid::ObjectIdentifier::new_unwrap("1.2.840.10045.3.1.7");
pub const ID_SECP384R1_OID: const_oid::ObjectIdentifier =
  const_oid::ObjectIdentifier::new_unwrap("1.3.132.0.34");

// Default HashAlgorithm for RSASSA-PSS-params (sha1)
//
// sha1 HashAlgorithm ::= {
//   algorithm   id-sha1,
//   parameters  SHA1Parameters : NULL
// }
//
// SHA1Parameters ::= NULL
static SHA1_HASH_ALGORITHM: Lazy<rsa::pkcs8::AlgorithmIdentifierRef<'static>> =
  Lazy::new(|| rsa::pkcs8::AlgorithmIdentifierRef {
    // id-sha1
    oid: ID_SHA1_OID,
    // NULL
    parameters: Some(asn1::AnyRef::from(asn1::Null)),
  });

// TODO(@littledivy): `pkcs8` should provide AlgorithmIdentifier to Any conversion.
static ENCODED_SHA1_HASH_ALGORITHM: Lazy<Vec<u8>> =
  Lazy::new(|| SHA1_HASH_ALGORITHM.to_der().unwrap());

// Default MaskGenAlgrithm for RSASSA-PSS-params (mgf1SHA1)
//
// mgf1SHA1 MaskGenAlgorithm ::= {
//   algorithm   id-mgf1,
//   parameters  HashAlgorithm : sha1
// }
static MGF1_SHA1_MASK_ALGORITHM: Lazy<
  rsa::pkcs8::AlgorithmIdentifierRef<'static>,
> = Lazy::new(|| rsa::pkcs8::AlgorithmIdentifierRef {
  // id-mgf1
  oid: ID_MFG1,
  // sha1
  parameters: Some(
    asn1::AnyRef::from_der(&ENCODED_SHA1_HASH_ALGORITHM).unwrap(),
  ),
});

pub const RSA_ENCRYPTION_OID: const_oid::ObjectIdentifier =
  const_oid::ObjectIdentifier::new_unwrap("1.2.840.113549.1.1.1");
pub const RSASSA_PSS_OID: const_oid::ObjectIdentifier =
  const_oid::ObjectIdentifier::new_unwrap("1.2.840.113549.1.1.10");
pub const DSA_OID: const_oid::ObjectIdentifier =
  const_oid::ObjectIdentifier::new_unwrap("1.2.840.10040.4.1");
pub const EC_OID: const_oid::ObjectIdentifier =
  const_oid::ObjectIdentifier::new_unwrap("1.2.840.10045.2.1");
pub const X25519_OID: const_oid::ObjectIdentifier =
  const_oid::ObjectIdentifier::new_unwrap("1.3.101.110");
pub const ED25519_OID: const_oid::ObjectIdentifier =
  const_oid::ObjectIdentifier::new_unwrap("1.3.101.112");
pub const DH_KEY_AGREEMENT_OID: const_oid::ObjectIdentifier =
  const_oid::ObjectIdentifier::new_unwrap("1.2.840.113549.1.3.1");

// The parameters field associated with OID id-RSASSA-PSS
// Defined in RFC 3447, section A.2.3
//
// RSASSA-PSS-params ::= SEQUENCE {
//   hashAlgorithm      [0] HashAlgorithm    DEFAULT sha1,
//   maskGenAlgorithm   [1] MaskGenAlgorithm DEFAULT mgf1SHA1,
//   saltLength         [2] INTEGER          DEFAULT 20,
//   trailerField       [3] TrailerField     DEFAULT trailerFieldBC
// }
pub struct RsaPssParameters<'a> {
  pub hash_algorithm: rsa::pkcs8::AlgorithmIdentifierRef<'a>,
  #[allow(dead_code)]
  pub mask_gen_algorithm: rsa::pkcs8::AlgorithmIdentifierRef<'a>,
  pub salt_length: u32,
}

// Context-specific tag number for hashAlgorithm.
const HASH_ALGORITHM_TAG: rsa::pkcs8::der::TagNumber =
  rsa::pkcs8::der::TagNumber::new(0);

// Context-specific tag number for maskGenAlgorithm.
const MASK_GEN_ALGORITHM_TAG: rsa::pkcs8::der::TagNumber =
  rsa::pkcs8::der::TagNumber::new(1);

// Context-specific tag number for saltLength.
const SALT_LENGTH_TAG: rsa::pkcs8::der::TagNumber =
  rsa::pkcs8::der::TagNumber::new(2);

impl<'a> TryFrom<rsa::pkcs8::der::asn1::AnyRef<'a>> for RsaPssParameters<'a> {
  type Error = rsa::pkcs8::der::Error;

  fn try_from(
    any: rsa::pkcs8::der::asn1::AnyRef<'a>,
  ) -> rsa::pkcs8::der::Result<RsaPssParameters> {
    any.sequence(|decoder| {
      let hash_algorithm = decoder
        .context_specific::<rsa::pkcs8::AlgorithmIdentifierRef>(
          HASH_ALGORITHM_TAG,
          pkcs8::der::TagMode::Explicit,
        )?
        .map(TryInto::try_into)
        .transpose()?
        .unwrap_or(*SHA1_HASH_ALGORITHM);

      let mask_gen_algorithm = decoder
        .context_specific::<rsa::pkcs8::AlgorithmIdentifierRef>(
          MASK_GEN_ALGORITHM_TAG,
          pkcs8::der::TagMode::Explicit,
        )?
        .map(TryInto::try_into)
        .transpose()?
        .unwrap_or(*MGF1_SHA1_MASK_ALGORITHM);

      let salt_length = decoder
        .context_specific::<u32>(
          SALT_LENGTH_TAG,
          pkcs8::der::TagMode::Explicit,
        )?
        .map(TryInto::try_into)
        .transpose()?
        .unwrap_or(20);

      Ok(Self {
        hash_algorithm,
        mask_gen_algorithm,
        salt_length,
      })
    })
  }
}

impl KeyObjectHandle {
  pub fn new_asymmetric_private_key_from_js(
    key: &[u8],
    format: &str,
    typ: &str,
    passphrase: Option<&[u8]>,
  ) -> Result<KeyObjectHandle, AnyError> {
    let document = match format {
      "pem" => {
        let pem = std::str::from_utf8(key).map_err(|err| {
          type_error(format!(
            "invalid PEM private key: not valid utf8 starting at byte {}",
            err.valid_up_to()
          ))
        })?;

        if let Some(passphrase) = passphrase {
          SecretDocument::from_pkcs8_encrypted_pem(pem, passphrase)
            .map_err(|_| type_error("invalid encrypted PEM private key"))?
        } else {
          let (label, doc) = SecretDocument::from_pem(pem)
            .map_err(|_| type_error("invalid PEM private key"))?;

          match label {
            EncryptedPrivateKeyInfo::PEM_LABEL => {
              return Err(type_error(
                "encrypted private key requires a passphrase to decrypt",
              ))
            }
            PrivateKeyInfo::PEM_LABEL => doc,
            rsa::pkcs1::RsaPrivateKey::PEM_LABEL => {
              SecretDocument::from_pkcs1_der(doc.as_bytes())
                .map_err(|_| type_error("invalid PKCS#1 private key"))?
            }
            sec1::EcPrivateKey::PEM_LABEL => {
              SecretDocument::from_sec1_der(doc.as_bytes())
                .map_err(|_| type_error("invalid SEC1 private key"))?
            }
            _ => {
              return Err(type_error(format!(
                "unsupported PEM label: {}",
                label
              )))
            }
          }
        }
      }
      "der" => match typ {
        "pkcs8" => {
          if let Some(passphrase) = passphrase {
            SecretDocument::from_pkcs8_encrypted_der(key, passphrase)
              .map_err(|_| type_error("invalid encrypted PKCS#8 private key"))?
          } else {
            SecretDocument::from_pkcs8_der(key)
              .map_err(|_| type_error("invalid PKCS#8 private key"))?
          }
        }
        "pkcs1" => {
          if passphrase.is_some() {
            return Err(type_error(
              "PKCS#1 private key does not support encryption with passphrase",
            ));
          }
          SecretDocument::from_pkcs1_der(key)
            .map_err(|_| type_error("invalid PKCS#1 private key"))?
        }
        "sec1" => {
          if passphrase.is_some() {
            return Err(type_error(
              "SEC1 private key does not support encryption with passphrase",
            ));
          }
          SecretDocument::from_sec1_der(key)
            .map_err(|_| type_error("invalid SEC1 private key"))?
        }
        _ => return Err(type_error(format!("unsupported key type: {}", typ))),
      },
      _ => {
        return Err(type_error(format!("unsupported key format: {}", format)))
      }
    };

    let pk_info = PrivateKeyInfo::try_from(document.as_bytes())
      .map_err(|_| type_error("invalid private key"))?;

    let alg = pk_info.algorithm.oid;
    let private_key = match alg {
      RSA_ENCRYPTION_OID => {
        let private_key =
          rsa::RsaPrivateKey::from_pkcs1_der(pk_info.private_key)
            .map_err(|_| type_error("invalid PKCS#1 private key"))?;
        AsymmetricPrivateKey::Rsa(private_key)
      }
      RSASSA_PSS_OID => {
        let parameters = pk_info
          .algorithm
          .parameters
          .ok_or_else(|| type_error("missing pss private key parameters"))?;
        let params = RsaPssParameters::try_from(parameters)
          .map_err(|_| type_error("malformed pss private key parameters"))?;

        let hash_alg = params.hash_algorithm;
        let hash_algorithm = match hash_alg.oid {
          ID_SHA1_OID => RsaPssHashAlgorithm::Sha1,
          ID_SHA256_OID => RsaPssHashAlgorithm::Sha256,
          ID_SHA384_OID => RsaPssHashAlgorithm::Sha384,
          ID_SHA512_OID => RsaPssHashAlgorithm::Sha512,
          _ => return Err(type_error("unsupported pss hash algorithm")),
        };

        let private_key =
          rsa::RsaPrivateKey::from_pkcs1_der(pk_info.private_key)
            .map_err(|_| type_error("invalid PKCS#1 private key"))?;
        AsymmetricPrivateKey::RsaPss(RsaPssPrivateKey {
          key: private_key,
          hash_algorithm,
          salt_length: params.salt_length,
        })
      }
      DSA_OID => {
        let private_key = dsa::SigningKey::try_from(pk_info)
          .map_err(|_| type_error("invalid DSA private key"))?;
        AsymmetricPrivateKey::Dsa(private_key)
      }
      EC_OID => {
        let named_curve = pk_info.algorithm.parameters_oid().map_err(|_| {
          type_error("malformed or missing named curve in ec parameters")
        })?;
        match named_curve {
          ID_SECP224R1_OID => {
            let secret_key =
              p224::SecretKey::from_sec1_der(pk_info.private_key)
                .map_err(|_| type_error("invalid SEC1 private key"))?;
            AsymmetricPrivateKey::Ec(EcPrivateKey::P224(secret_key))
          }
          ID_SECP256R1_OID => {
            let secret_key =
              p256::SecretKey::from_sec1_der(pk_info.private_key)
                .map_err(|_| type_error("invalid SEC1 private key"))?;
            AsymmetricPrivateKey::Ec(EcPrivateKey::P256(secret_key))
          }
          ID_SECP384R1_OID => {
            let secret_key =
              p384::SecretKey::from_sec1_der(pk_info.private_key)
                .map_err(|_| type_error("invalid SEC1 private key"))?;
            AsymmetricPrivateKey::Ec(EcPrivateKey::P384(secret_key))
          }
          _ => return Err(type_error("unsupported ec named curve")),
        }
      }
      X25519_OID => {
        let mut bytes = [0; 32];
        if pk_info.private_key.len() != 32 {
          return Err(type_error("x25519 private key is the wrong length"));
        }
        bytes.copy_from_slice(pk_info.private_key);
        AsymmetricPrivateKey::X25519(x25519_dalek::StaticSecret::from(bytes))
      }
      ED25519_OID => {
        let mut bytes = [0; 32];
        if pk_info.private_key.len() != 32 {
          return Err(type_error("x25519 private key is the wrong length"));
        }
        bytes.copy_from_slice(pk_info.private_key);
        AsymmetricPrivateKey::Ed25519(ed25519_dalek::SigningKey::from(bytes))
      }
      DH_KEY_AGREEMENT_OID => AsymmetricPrivateKey::Dh(
        pk_info.private_key.to_vec().into_boxed_slice(),
      ),
      _ => return Err(type_error("unsupported private key oid")),
    };

    Ok(KeyObjectHandle::AsymmetricPrivateKey(private_key))
  }

  pub fn new_asymmetric_public_key_from_js(
    key: &[u8],
    format: &str,
    typ: &str,
    _passphrase: Option<&[u8]>,
  ) -> Result<KeyObjectHandle, AnyError> {
    let document = match format {
      "pem" => {
        let pem = std::str::from_utf8(key).map_err(|err| {
          type_error(format!(
            "invalid PEM public key: not valid utf8 starting at byte {}",
            err.valid_up_to()
          ))
        })?;

        let (label, document) = Document::from_pem(pem)
          .map_err(|_| type_error("invalid PEM public key"))?;

        match label {
          SubjectPublicKeyInfoRef::PEM_LABEL => document,
          rsa::pkcs1::RsaPublicKey::PEM_LABEL => {
            Document::from_pkcs1_der(document.as_bytes())
              .map_err(|_| type_error("invalid PKCS#1 public key"))?
          }
          EncryptedPrivateKeyInfo::PEM_LABEL => {
            // FIXME
            return Err(type_error(
              "deriving public key from encrypted private key",
            ));
          }
          PrivateKeyInfo::PEM_LABEL => {
            // FIXME
            return Err(type_error("public key cannot be a private key"));
          }
          sec1::EcPrivateKey::PEM_LABEL => {
            // FIXME
            return Err(type_error("deriving public key from ec private key"));
          }
          rsa::pkcs1::RsaPrivateKey::PEM_LABEL => {
            // FIXME
            return Err(type_error("deriving public key from rsa private key"));
          }
          // TODO: handle x509 certificates as public keys
          _ => {
            return Err(type_error(format!("unsupported PEM label: {}", label)))
          }
        }
      }
      "der" => match typ {
        "pkcs1" => Document::from_pkcs1_der(key)
          .map_err(|_| type_error("invalid PKCS#1 public key"))?,
        "spki" => Document::from_public_key_der(key)
          .map_err(|_| type_error("invalid SPKI public key"))?,
        _ => return Err(type_error(format!("unsupported key type: {}", typ))),
      },
      _ => {
        return Err(type_error(format!("unsupported key format: {}", format)))
      }
    };

    let spki = SubjectPublicKeyInfoRef::try_from(document.as_bytes())?;

    let public_key = match spki.algorithm.oid {
      RSA_ENCRYPTION_OID => {
        let public_key = RsaPublicKey::from_pkcs1_der(
          spki.subject_public_key.as_bytes().unwrap(),
        )?;
        AsymmetricPublicKey::Rsa(public_key)
      }
      RSASSA_PSS_OID => {
        let parameters = spki
          .algorithm
          .parameters
          .ok_or_else(|| type_error("missing pss public key parameters"))?;
        let params = RsaPssParameters::try_from(parameters)
          .map_err(|_| type_error("malformed pss public key parameters"))?;

        let hash_alg = params.hash_algorithm;
        let hash_algorithm = match hash_alg.oid {
          ID_SHA1_OID => RsaPssHashAlgorithm::Sha1,
          ID_SHA256_OID => RsaPssHashAlgorithm::Sha256,
          ID_SHA384_OID => RsaPssHashAlgorithm::Sha384,
          ID_SHA512_OID => RsaPssHashAlgorithm::Sha512,
          _ => return Err(type_error("unsupported pss hash algorithm")),
        };

        let public_key = RsaPublicKey::from_pkcs1_der(
          spki.subject_public_key.as_bytes().unwrap(),
        )?;
        AsymmetricPublicKey::RsaPss(RsaPssPublicKey {
          key: public_key,
          hash_algorithm,
          salt_length: params.salt_length,
        })
      }
      DSA_OID => {
        let verifying_key = dsa::VerifyingKey::try_from(spki)
          .map_err(|_| type_error("malformed DSS public key"))?;
        AsymmetricPublicKey::Dsa(verifying_key)
      }
      EC_OID => {
        let named_curve = spki.algorithm.parameters_oid().map_err(|_| {
          type_error("malformed or missing named curve in ec parameters")
        })?;
        let data = spki.subject_public_key.as_bytes().ok_or_else(|| {
          type_error("malformed or missing public key in ec spki")
        })?;

        match named_curve {
          ID_SECP224R1_OID => {
            let public_key = p224::PublicKey::from_sec1_bytes(data)?;
            AsymmetricPublicKey::Ec(EcPublicKey::P224(public_key))
          }
          ID_SECP256R1_OID => {
            let public_key = p256::PublicKey::from_sec1_bytes(data)?;
            AsymmetricPublicKey::Ec(EcPublicKey::P256(public_key))
          }
          ID_SECP384R1_OID => {
            let public_key = p384::PublicKey::from_sec1_bytes(data)?;
            AsymmetricPublicKey::Ec(EcPublicKey::P384(public_key))
          }
          _ => return Err(type_error("unsupported ec named curve")),
        }
      }
      X25519_OID => {
        let mut bytes = [0; 32];
        let data = spki.subject_public_key.as_bytes().ok_or_else(|| {
          type_error("malformed or missing public key in x25519 spki")
        })?;
        if data.len() != 32 {
          return Err(type_error("x25519 public key is the wrong length"));
        }
        bytes.copy_from_slice(data);
        AsymmetricPublicKey::X25519(x25519_dalek::PublicKey::from(bytes))
      }
      ED25519_OID => {
        let mut bytes = [0; 32];
        let data = spki.subject_public_key.as_bytes().ok_or_else(|| {
          type_error("malformed or missing public key in ed25519 spki")
        })?;
        if data.len() != 32 {
          return Err(type_error("ed25519 public key is the wrong length"));
        }
        bytes.copy_from_slice(data);
        let verifying_key = ed25519_dalek::VerifyingKey::from_bytes(&bytes)
          .map_err(|_| type_error("ed25519 public key is malformed"))?;
        AsymmetricPublicKey::Ed25519(verifying_key)
      }
      DH_KEY_AGREEMENT_OID => AsymmetricPublicKey::Dh(
        spki
          .subject_public_key
          .as_bytes()
          .unwrap()
          .to_vec()
          .into_boxed_slice(),
      ),
      _ => return Err(type_error("unsupported public key oid")),
    };

    Ok(KeyObjectHandle::AsymmetricPublicKey(public_key))
  }
}

impl AsymmetricPublicKey {
  fn export_der(&self, typ: &str) -> Result<Box<[u8]>, AnyError> {
    match typ {
      "pkcs1" => match self {
        AsymmetricPublicKey::Rsa(key) => {
          let der = key
            .to_pkcs1_der()
            .map_err(|_| type_error("invalid RSA public key"))?
            .into_vec()
            .into_boxed_slice();
          Ok(der)
        }
        _ => {
          return Err(type_error(
            "exporting non-RSA public key as PKCS#1 is not supported",
          ))
        }
      },
      "spki" => {
        let der = match self {
          AsymmetricPublicKey::Rsa(key) => key
            .to_public_key_der()
            .map_err(|_| type_error("invalid RSA public key"))?,
          AsymmetricPublicKey::RsaPss(_key) => {
            return Err(generic_error(
              "exporting RSA-PSS public key as SPKI is not supported yet",
            ))
          }
          AsymmetricPublicKey::Dsa(key) => key
            .to_public_key_der()
            .map_err(|_| type_error("invalid DSA public key"))?,
          AsymmetricPublicKey::Ec(key) => {
            let (sec1, oid) = match key {
              EcPublicKey::P224(key) => (key.to_sec1_bytes(), ID_SECP224R1_OID),
              EcPublicKey::P256(key) => (key.to_sec1_bytes(), ID_SECP256R1_OID),
              EcPublicKey::P384(key) => (key.to_sec1_bytes(), ID_SECP384R1_OID),
            };

            let spki = SubjectPublicKeyInfoRef {
              algorithm: rsa::pkcs8::AlgorithmIdentifierRef {
                oid: EC_OID,
                parameters: Some(asn1::AnyRef::from(&oid)),
              },
              subject_public_key: BitStringRef::from_bytes(&sec1)
                .map_err(|_| type_error("invalid EC public key"))?,
            };

            let der = spki
              .to_der()
              .map_err(|_| type_error("invalid EC public key"))?
              .into_boxed_slice();
            return Ok(der);
          }
          AsymmetricPublicKey::X25519(key) => {
            let spki = SubjectPublicKeyInfoRef {
              algorithm: rsa::pkcs8::AlgorithmIdentifierRef {
                oid: X25519_OID,
                parameters: None,
              },
              subject_public_key: BitStringRef::from_bytes(key.as_bytes())
                .map_err(|_| type_error("invalid X25519 public key"))?,
            };

            let der = spki
              .to_der()
              .map_err(|_| type_error("invalid X25519 public key"))?
              .into_boxed_slice();
            return Ok(der);
          }
          AsymmetricPublicKey::Ed25519(key) => {
            let spki = SubjectPublicKeyInfoRef {
              algorithm: rsa::pkcs8::AlgorithmIdentifierRef {
                oid: ED25519_OID,
                parameters: None,
              },
              subject_public_key: BitStringRef::from_bytes(key.as_bytes())
                .map_err(|_| type_error("invalid Ed25519 public key"))?,
            };

            let der = spki
              .to_der()
              .map_err(|_| type_error("invalid Ed25519 public key"))?
              .into_boxed_slice();
            return Ok(der);
          }
          AsymmetricPublicKey::Dh(key) => {
            let spki = SubjectPublicKeyInfoRef {
              algorithm: rsa::pkcs8::AlgorithmIdentifierRef {
                oid: DH_KEY_AGREEMENT_OID,
                parameters: None,
              },
              subject_public_key: BitStringRef::from_bytes(key)
                .map_err(|_| type_error("invalid DH public key"))?,
            };
            let der = spki
              .to_der()
              .map_err(|_| type_error("invalid DH public key"))?
              .into_boxed_slice();
            return Ok(der);
          }
        };
        Ok(der.into_vec().into_boxed_slice())
      }
      _ => return Err(type_error(format!("unsupported key type: {}", typ))),
    }
  }
}

impl AsymmetricPrivateKey {
  fn export_der(&self, typ: &str) -> Result<Box<[u8]>, AnyError> {
    match typ {
      "pkcs1" => match self {
        AsymmetricPrivateKey::Rsa(key) => {
          let der = key
            .to_pkcs1_der()
            .map_err(|_| type_error("invalid RSA private key"))?
            .to_bytes()
            .to_vec()
            .into_boxed_slice();
          Ok(der)
        }
        _ => {
          return Err(type_error(
            "exporting non-RSA private key as PKCS#1 is not supported",
          ))
        }
      },
      "sec1" => match self {
        AsymmetricPrivateKey::Ec(key) => {
          let sec1 = match key {
            EcPrivateKey::P224(key) => key.to_sec1_der(),
            EcPrivateKey::P256(key) => key.to_sec1_der(),
            EcPrivateKey::P384(key) => key.to_sec1_der(),
          }
          .map_err(|_| type_error("invalid EC private key"))?;
          Ok(sec1.to_vec().into_boxed_slice())
        }
        _ => {
          return Err(type_error(
            "exporting non-EC private key as SEC1 is not supported",
          ))
        }
      },
      "pkcs8" => {
        let der = match self {
          AsymmetricPrivateKey::Rsa(key) => {
            let document = key
              .to_pkcs8_der()
              .map_err(|_| type_error("invalid RSA private key"))?;
            document.to_bytes().to_vec().into_boxed_slice()
          }
          AsymmetricPrivateKey::RsaPss(_key) => {
            return Err(generic_error(
              "exporting RSA-PSS private key as PKCS#8 is not supported yet",
            ))
          }
          AsymmetricPrivateKey::Dsa(key) => {
            let document = key
              .to_pkcs8_der()
              .map_err(|_| type_error("invalid DSA private key"))?;
            document.to_bytes().to_vec().into_boxed_slice()
          }
          AsymmetricPrivateKey::Ec(key) => {
            let document = match key {
              EcPrivateKey::P224(key) => key.to_pkcs8_der(),
              EcPrivateKey::P256(key) => key.to_pkcs8_der(),
              EcPrivateKey::P384(key) => key.to_pkcs8_der(),
            }
            .map_err(|_| type_error("invalid EC private key"))?;
            document.to_bytes().to_vec().into_boxed_slice()
          }
          AsymmetricPrivateKey::X25519(key) => {
            let private_key = PrivateKeyInfo {
              algorithm: rsa::pkcs8::AlgorithmIdentifierRef {
                oid: X25519_OID,
                parameters: None,
              },
              private_key: key.as_bytes(),
              public_key: None,
            };

            let der = private_key
              .to_der()
              .map_err(|_| type_error("invalid X25519 private key"))?
              .into_boxed_slice();
            return Ok(der);
          }
          AsymmetricPrivateKey::Ed25519(key) => {
            let private_key = PrivateKeyInfo {
              algorithm: rsa::pkcs8::AlgorithmIdentifierRef {
                oid: ED25519_OID,
                parameters: None,
              },
              private_key: key.as_bytes(),
              public_key: None,
            };

            private_key
              .to_der()
              .map_err(|_| type_error("invalid ED25519 private key"))?
              .into_boxed_slice()
          }
          AsymmetricPrivateKey::Dh(key) => {
            let private_key = PrivateKeyInfo {
              algorithm: rsa::pkcs8::AlgorithmIdentifierRef {
                oid: DH_KEY_AGREEMENT_OID,
                parameters: None,
              },
              private_key: &*key,
              public_key: None,
            };

            private_key
              .to_der()
              .map_err(|_| type_error("invalid DH private key"))?
              .into_boxed_slice()
          }
        };

        Ok(der)
      }
      _ => return Err(type_error(format!("unsupported key type: {}", typ))),
    }
  }
}

#[op2]
#[cppgc]
pub fn op_node_create_private_key(
  #[buffer] key: &[u8],
  #[string] format: &str,
  #[string] typ: &str,
  #[buffer] passphrase: Option<&[u8]>,
) -> Result<KeyObjectHandle, AnyError> {
  KeyObjectHandle::new_asymmetric_private_key_from_js(
    key, format, typ, passphrase,
  )
}

#[op2]
#[cppgc]
pub fn op_node_create_public_key(
  #[buffer] key: &[u8],
  #[string] format: &str,
  #[string] typ: &str,
  #[buffer] passphrase: Option<&[u8]>,
) -> Result<KeyObjectHandle, AnyError> {
  KeyObjectHandle::new_asymmetric_public_key_from_js(
    key, format, typ, passphrase,
  )
}

#[op2]
#[cppgc]
pub fn op_node_create_secret_key(
  #[buffer(copy)] key: Box<[u8]>,
) -> KeyObjectHandle {
  KeyObjectHandle::SecretKey(key)
}

#[op2]
#[string]
pub fn op_node_get_asymmetric_key_type(
  #[cppgc] handle: &KeyObjectHandle,
) -> Result<&'static str, AnyError> {
  match handle {
    KeyObjectHandle::AsymmetricPrivateKey(AsymmetricPrivateKey::Rsa(_))
    | KeyObjectHandle::AsymmetricPublicKey(AsymmetricPublicKey::Rsa(_)) => {
      Ok("rsa")
    }
    KeyObjectHandle::AsymmetricPrivateKey(AsymmetricPrivateKey::RsaPss(_))
    | KeyObjectHandle::AsymmetricPublicKey(AsymmetricPublicKey::RsaPss(_)) => {
      Ok("rsa-pss")
    }
    KeyObjectHandle::AsymmetricPrivateKey(AsymmetricPrivateKey::Dsa(_))
    | KeyObjectHandle::AsymmetricPublicKey(AsymmetricPublicKey::Dsa(_)) => {
      Ok("dsa")
    }
    KeyObjectHandle::AsymmetricPrivateKey(AsymmetricPrivateKey::Ec(_))
    | KeyObjectHandle::AsymmetricPublicKey(AsymmetricPublicKey::Ec(_)) => {
      Ok("ec")
    }
    KeyObjectHandle::AsymmetricPrivateKey(AsymmetricPrivateKey::X25519(_))
    | KeyObjectHandle::AsymmetricPublicKey(AsymmetricPublicKey::X25519(_)) => {
      Ok("x25519")
    }
    KeyObjectHandle::AsymmetricPrivateKey(AsymmetricPrivateKey::Ed25519(_))
    | KeyObjectHandle::AsymmetricPublicKey(AsymmetricPublicKey::Ed25519(_)) => {
      Ok("ed25519")
    }
    KeyObjectHandle::AsymmetricPrivateKey(AsymmetricPrivateKey::Dh(_))
    | KeyObjectHandle::AsymmetricPublicKey(AsymmetricPublicKey::Dh(_)) => {
      Ok("dh")
    }
    KeyObjectHandle::SecretKey(_) => {
      Err(type_error("symmetric key is not an asymmetric key"))
    }
  }
}

#[derive(serde::Serialize)]
#[serde(untagged)]
pub enum AsymmetricKeyDetails {
  #[serde(rename_all = "camelCase")]
  Rsa {
    modulus_length: usize,
    public_exponent: V8BigInt,
  },
  #[serde(rename_all = "camelCase")]
  RsaPss {
    modulus_length: usize,
    public_exponent: V8BigInt,
    hash_algorithm: &'static str,
    salt_length: u32,
  },
  #[serde(rename_all = "camelCase")]
  Dsa {
    modulus_length: usize,
    divisor_length: usize,
  },
  #[serde(rename_all = "camelCase")]
  Ec {
    named_curve: &'static str,
  },
  X25519,
  Ed25519,
  Dh,
}

#[op2]
#[serde]
pub fn op_node_get_asymmetric_key_details(
  #[cppgc] handle: &KeyObjectHandle,
) -> Result<AsymmetricKeyDetails, AnyError> {
  match handle {
    KeyObjectHandle::AsymmetricPrivateKey(private_key) => match private_key {
      AsymmetricPrivateKey::Rsa(key) => {
        let modulus_length = key.n().bits();
        let public_exponent =
          BigInt::from_bytes_be(num_bigint::Sign::Plus, &key.e().to_bytes_be());
        Ok(AsymmetricKeyDetails::Rsa {
          modulus_length,
          public_exponent: V8BigInt::from(public_exponent),
        })
      }
      AsymmetricPrivateKey::RsaPss(key) => {
        let modulus_length = key.key.n().bits();
        let public_exponent = BigInt::from_bytes_be(
          num_bigint::Sign::Plus,
          &key.key.e().to_bytes_be(),
        );
        let hash_algorithm = match key.hash_algorithm {
          RsaPssHashAlgorithm::Sha1 => "sha1",
          RsaPssHashAlgorithm::Sha256 => "sha256",
          RsaPssHashAlgorithm::Sha384 => "sha384",
          RsaPssHashAlgorithm::Sha512 => "sha512",
        };
        Ok(AsymmetricKeyDetails::RsaPss {
          modulus_length,
          public_exponent: V8BigInt::from(public_exponent),
          hash_algorithm,
          salt_length: key.salt_length,
        })
      }
      AsymmetricPrivateKey::Dsa(key) => {
        let components = key.verifying_key().components();
        let modulus_length = components.p().bits();
        let divisor_length = components.q().bits();
        Ok(AsymmetricKeyDetails::Dsa {
          modulus_length,
          divisor_length,
        })
      }
      AsymmetricPrivateKey::Ec(key) => {
        let named_curve = match key {
          EcPrivateKey::P224(_) => "p224",
          EcPrivateKey::P256(_) => "p256",
          EcPrivateKey::P384(_) => "p384",
        };
        Ok(AsymmetricKeyDetails::Ec { named_curve })
      }
      AsymmetricPrivateKey::X25519(_) => Ok(AsymmetricKeyDetails::X25519),
      AsymmetricPrivateKey::Ed25519(_) => Ok(AsymmetricKeyDetails::Ed25519),
      AsymmetricPrivateKey::Dh(_) => Ok(AsymmetricKeyDetails::Dh),
    },
    KeyObjectHandle::AsymmetricPublicKey(public_key) => match public_key {
      AsymmetricPublicKey::Rsa(key) => {
        let modulus_length = key.n().bits();
        let public_exponent =
          BigInt::from_bytes_be(num_bigint::Sign::Plus, &key.e().to_bytes_be());
        Ok(AsymmetricKeyDetails::Rsa {
          modulus_length,
          public_exponent: V8BigInt::from(public_exponent),
        })
      }
      AsymmetricPublicKey::RsaPss(key) => {
        let modulus_length = key.key.n().bits();
        let public_exponent = BigInt::from_bytes_be(
          num_bigint::Sign::Plus,
          &key.key.e().to_bytes_be(),
        );
        let hash_algorithm = match key.hash_algorithm {
          RsaPssHashAlgorithm::Sha1 => "sha1",
          RsaPssHashAlgorithm::Sha256 => "sha256",
          RsaPssHashAlgorithm::Sha384 => "sha384",
          RsaPssHashAlgorithm::Sha512 => "sha512",
        };
        Ok(AsymmetricKeyDetails::RsaPss {
          modulus_length,
          public_exponent: V8BigInt::from(public_exponent),
          hash_algorithm,
          salt_length: key.salt_length,
        })
      }
      AsymmetricPublicKey::Dsa(key) => {
        let components = key.components();
        let modulus_length = components.p().bits();
        let divisor_length = components.q().bits();
        Ok(AsymmetricKeyDetails::Dsa {
          modulus_length,
          divisor_length,
        })
      }
      AsymmetricPublicKey::Ec(key) => {
        let named_curve = match key {
          EcPublicKey::P224(_) => "p224",
          EcPublicKey::P256(_) => "p256",
          EcPublicKey::P384(_) => "p384",
        };
        Ok(AsymmetricKeyDetails::Ec { named_curve })
      }
      AsymmetricPublicKey::X25519(_) => Ok(AsymmetricKeyDetails::X25519),
      AsymmetricPublicKey::Ed25519(_) => Ok(AsymmetricKeyDetails::Ed25519),
      AsymmetricPublicKey::Dh(_) => Ok(AsymmetricKeyDetails::Dh),
    },
    KeyObjectHandle::SecretKey(_) => {
      Err(type_error("symmetric key is not an asymmetric key"))
    }
  }
}

#[op2(fast)]
#[smi]
pub fn op_node_get_symmetric_key_size(
  #[cppgc] handle: &KeyObjectHandle,
) -> Result<usize, AnyError> {
  match handle {
    KeyObjectHandle::AsymmetricPrivateKey(_) => {
      Err(type_error("asymmetric key is not a symmetric key"))
    }
    KeyObjectHandle::AsymmetricPublicKey(_) => {
      Err(type_error("asymmetric key is not a symmetric key"))
    }
    KeyObjectHandle::SecretKey(key) => Ok(key.len() * 8),
  }
}

#[op2]
#[cppgc]
pub fn op_node_generate_secret_key(#[smi] len: usize) -> KeyObjectHandle {
  let mut key = vec![0u8; len];
  thread_rng().fill_bytes(&mut key);
  KeyObjectHandle::SecretKey(key.into_boxed_slice())
}

#[op2(async)]
#[cppgc]
pub async fn op_node_generate_secret_key_async(
  #[smi] len: usize,
) -> KeyObjectHandle {
  spawn_blocking(move || {
    let mut key = vec![0u8; len];
    thread_rng().fill_bytes(&mut key);
    KeyObjectHandle::SecretKey(key.into_boxed_slice())
  })
  .await
  .unwrap()
}

struct KeyObjectHandlePair {
  private_key: RefCell<Option<KeyObjectHandle>>,
  public_key: RefCell<Option<KeyObjectHandle>>,
}

impl GarbageCollected for KeyObjectHandlePair {}

impl KeyObjectHandlePair {
  pub fn new(
    private_key: AsymmetricPrivateKey,
    public_key: AsymmetricPublicKey,
  ) -> Self {
    Self {
      private_key: RefCell::new(Some(KeyObjectHandle::AsymmetricPrivateKey(
        private_key,
      ))),
      public_key: RefCell::new(Some(KeyObjectHandle::AsymmetricPublicKey(
        public_key,
      ))),
    }
  }
}

#[op2]
#[cppgc]
pub fn op_node_get_public_key_from_pair(
  #[cppgc] pair: &KeyObjectHandlePair,
) -> Option<KeyObjectHandle> {
  pair.public_key.borrow_mut().take()
}

#[op2]
#[cppgc]
pub fn op_node_get_private_key_from_pair(
  #[cppgc] pair: &KeyObjectHandlePair,
) -> Option<KeyObjectHandle> {
  pair.private_key.borrow_mut().take()
}

fn generate_rsa(
  modulus_length: usize,
  public_exponent: usize,
) -> KeyObjectHandlePair {
  let private_key = RsaPrivateKey::new_with_exp(
    &mut thread_rng(),
    modulus_length,
    &rsa::BigUint::from_usize(public_exponent).unwrap(),
  )
  .unwrap();

  let private_key = AsymmetricPrivateKey::Rsa(private_key);
  let public_key = private_key.to_public_key();

  KeyObjectHandlePair::new(private_key, public_key)
}

#[op2]
#[cppgc]
pub fn op_node_generate_rsa_key(
  #[smi] modulus_length: usize,
  #[smi] public_exponent: usize,
) -> KeyObjectHandlePair {
  generate_rsa(modulus_length, public_exponent)
}

#[op2(async)]
#[cppgc]
pub async fn op_node_generate_rsa_key_async(
  #[smi] modulus_length: usize,
  #[smi] public_exponent: usize,
) -> KeyObjectHandlePair {
  spawn_blocking(move || generate_rsa(modulus_length, public_exponent))
    .await
    .unwrap()
}

fn dsa_generate(
  modulus_length: usize,
  divisor_length: usize,
) -> Result<KeyObjectHandlePair, AnyError> {
  let mut rng = rand::thread_rng();
  use dsa::Components;
  use dsa::KeySize;
  use dsa::SigningKey;

  let key_size = match (modulus_length, divisor_length) {
    #[allow(deprecated)]
    (1024, 160) => KeySize::DSA_1024_160,
    (2048, 224) => KeySize::DSA_2048_224,
    (2048, 256) => KeySize::DSA_2048_256,
    (3072, 256) => KeySize::DSA_3072_256,
    _ => {
      return Err(type_error(
        "Invalid modulusLength+divisorLength combination",
      ))
    }
  };
  let components = Components::generate(&mut rng, key_size);
  let signing_key = SigningKey::generate(&mut rng, components);
  let private_key = AsymmetricPrivateKey::Dsa(signing_key);
  let public_key = private_key.to_public_key();

  Ok(KeyObjectHandlePair::new(private_key, public_key))
}

#[op2]
#[cppgc]
pub fn op_node_generate_dsa_key(
  #[smi] modulus_length: usize,
  #[smi] divisor_length: usize,
) -> Result<KeyObjectHandlePair, AnyError> {
  dsa_generate(modulus_length, divisor_length)
}

#[op2(async)]
#[cppgc]
pub async fn op_node_generate_dsa_key_async(
  #[smi] modulus_length: usize,
  #[smi] divisor_length: usize,
) -> Result<KeyObjectHandlePair, AnyError> {
  spawn_blocking(move || dsa_generate(modulus_length, divisor_length))
    .await
    .unwrap()
}

fn ec_generate(named_curve: &str) -> Result<KeyObjectHandlePair, AnyError> {
  let mut rng = rand::thread_rng();
  // TODO(@littledivy): Support public key point encoding.
  // Default is uncompressed.
  let private_key = match named_curve {
    "P-224" | "prime224v1" | "secp224r1" => {
      let key = p224::SecretKey::random(&mut rng);
      AsymmetricPrivateKey::Ec(EcPrivateKey::P224(key))
    }
    "P-256" | "prime256v1" | "secp256r1" => {
      let key = p256::SecretKey::random(&mut rng);
      AsymmetricPrivateKey::Ec(EcPrivateKey::P256(key))
    }
    "P-384" | "prime384v1" | "secp384r1" => {
      let key = p384::SecretKey::random(&mut rng);
      AsymmetricPrivateKey::Ec(EcPrivateKey::P384(key))
    }
    _ => {
      return Err(type_error(format!(
        "unsupported named curve: {}",
        named_curve
      )))
    }
  };
  let public_key = private_key.to_public_key();
  Ok(KeyObjectHandlePair::new(private_key, public_key))
}

#[op2]
#[cppgc]
pub fn op_node_generate_ec_key(
  #[string] named_curve: &str,
) -> Result<KeyObjectHandlePair, AnyError> {
  ec_generate(named_curve)
}

#[op2(async)]
#[cppgc]
pub async fn op_node_generate_ec_key_async(
  #[string] named_curve: String,
) -> Result<KeyObjectHandlePair, AnyError> {
  spawn_blocking(move || ec_generate(&named_curve))
    .await
    .unwrap()
}

fn x25519_generate() -> KeyObjectHandlePair {
  let keypair = x25519_dalek::StaticSecret::random_from_rng(&mut thread_rng());
  let private_key = AsymmetricPrivateKey::X25519(keypair);
  let public_key = private_key.to_public_key();
  KeyObjectHandlePair::new(private_key, public_key)
}

#[op2]
#[cppgc]
pub fn op_node_generate_x25519_key() -> KeyObjectHandlePair {
  x25519_generate()
}

#[op2(async)]
#[cppgc]
pub async fn op_node_generate_x25519_key_async() -> KeyObjectHandlePair {
  spawn_blocking(x25519_generate).await.unwrap()
}

fn ed25519_generate() -> KeyObjectHandlePair {
  let keypair = ed25519_dalek::SigningKey::generate(&mut thread_rng());
  let private_key = AsymmetricPrivateKey::Ed25519(keypair);
  let public_key = private_key.to_public_key();
  KeyObjectHandlePair::new(private_key, public_key)
}

#[op2]
#[cppgc]
pub fn op_node_generate_ed25519_key() -> KeyObjectHandlePair {
  ed25519_generate()
}

#[op2(async)]
#[cppgc]
pub async fn op_node_generate_ed25519_key_async() -> KeyObjectHandlePair {
  spawn_blocking(ed25519_generate).await.unwrap()
}

#[op2]
#[buffer]
pub fn op_node_export_secret_key(
  #[cppgc] handle: &KeyObjectHandle,
) -> Result<Box<[u8]>, AnyError> {
  let key = handle
    .as_secret_key()
    .ok_or_else(|| type_error("key is not a secret key"))?;
  Ok(key.to_vec().into_boxed_slice())
}

#[op2]
#[string]
pub fn op_node_export_secret_key_b64url(
  #[cppgc] handle: &KeyObjectHandle,
) -> Result<String, AnyError> {
  let key = handle
    .as_secret_key()
    .ok_or_else(|| type_error("key is not a secret key"))?;
  Ok(base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(key))
}

#[op2]
#[string]
pub fn op_node_export_public_key_pem(
  #[cppgc] handle: &KeyObjectHandle,
  #[string] typ: &str,
) -> Result<String, AnyError> {
  let public_key = handle
    .as_public_key()
    .ok_or_else(|| type_error("key is not an asymmetric public key"))?;
  let data = public_key.export_der(typ)?;

  let label = match typ {
    "pkcs1" => "RSA PUBLIC KEY",
    "spki" => "PUBLIC KEY",
    _ => unreachable!("export_der would have errored"),
  };

  let mut out = vec![0; 2048];
  let mut writer = PemWriter::new(label, LineEnding::LF, &mut out)?;
  writer.write(&data)?;
  let len = writer.finish()?;
  out.truncate(len);

  Ok(String::from_utf8(out).expect("invalid pem is not possible"))
}

#[op2]
#[buffer]
pub fn op_node_export_public_key_der(
  #[cppgc] handle: &KeyObjectHandle,
  #[string] typ: &str,
) -> Result<Box<[u8]>, AnyError> {
  let public_key = handle
    .as_public_key()
    .ok_or_else(|| type_error("key is not an asymmetric public key"))?;
  public_key.export_der(typ)
}

#[op2]
#[string]
pub fn op_node_export_private_key_pem(
  #[cppgc] handle: &KeyObjectHandle,
  #[string] typ: &str,
) -> Result<String, AnyError> {
  let private_key = handle
    .as_private_key()
    .ok_or_else(|| type_error("key is not an asymmetric private key"))?;
  let data = private_key.export_der(typ)?;

  let label = match typ {
    "pkcs1" => "RSA PRIVATE KEY",
    "pkcs8" => "PRIVATE KEY",
    "sec1" => "EC PRIVATE KEY",
    _ => unreachable!("export_der would have errored"),
  };

  let mut out = vec![0; 2048];
  let mut writer = PemWriter::new(label, LineEnding::LF, &mut out)?;
  writer.write(&data)?;
  let len = writer.finish()?;
  out.truncate(len);

  Ok(String::from_utf8(out).expect("invalid pem is not possible"))
}

#[op2]
#[buffer]
pub fn op_node_export_private_key_der(
  #[cppgc] handle: &KeyObjectHandle,
  #[string] typ: &str,
) -> Result<Box<[u8]>, AnyError> {
  let private_key = handle
    .as_private_key()
    .ok_or_else(|| type_error("key is not an asymmetric private key"))?;
  private_key.export_der(typ)
}

#[op2]
#[string]
pub fn op_node_key_type(#[cppgc] handle: &KeyObjectHandle) -> &'static str {
  match handle {
    KeyObjectHandle::AsymmetricPrivateKey(_) => "private",
    KeyObjectHandle::AsymmetricPublicKey(_) => "public",
    KeyObjectHandle::SecretKey(_) => "secret",
  }
}

#[op2]
#[cppgc]
pub fn op_node_derive_public_key_from_private_key(
  #[cppgc] handle: &KeyObjectHandle,
) -> Result<KeyObjectHandle, AnyError> {
  let Some(private_key) = handle.as_private_key() else {
    return Err(type_error("expected private key"));
  };

  Ok(KeyObjectHandle::AsymmetricPublicKey(
    private_key.to_public_key(),
  ))
}
