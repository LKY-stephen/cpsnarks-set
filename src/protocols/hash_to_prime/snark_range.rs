//! LegoGroth16-based range proof.

use crate::{
    commitments::pedersen::PedersenCommitment,
    parameters::Parameters,
    protocols::{
        hash_to_prime::{
            channel::{HashToPrimeProverChannel, HashToPrimeVerifierChannel},
            CRSHashToPrime, HashToPrimeError, HashToPrimeProtocol, Statement, Witness,
        },
        ProofError, SetupError, VerificationError,
    },
    utils::integer_to_bigint_mod_q,
};
use ark_ec::{AffineCurve, PairingEngine, ProjectiveCurve};
use ark_ff::{PrimeField, UniformRand};
use ark_r1cs_std::{
    alloc::{AllocVar, AllocationMode},
    bits::ToBitsGadget,
    boolean::Boolean,
    eq::EqGadget,
    fields::fp::FpVar,
    Assignment,
};
use ark_relations::r1cs::{ConstraintSynthesizer, ConstraintSystemRef, SynthesisError};
use rand::Rng;
use rug::Integer;
use std::ops::Sub;

pub struct HashToPrimeCircuit<E: PairingEngine> {
    required_bit_size: u16,
    value: Option<E::Fr>,
}

impl<E: PairingEngine> ConstraintSynthesizer<E::Fr> for HashToPrimeCircuit<E> {
    fn generate_constraints(self, cs: ConstraintSystemRef<E::Fr>) -> Result<(), SynthesisError> {
        let f = FpVar::new_variable(
            ark_relations::ns!(cs, "alloc value"),
            || self.value.get(),
            AllocationMode::Input,
        )?;
        // big-endian bits
        let bits = f.to_non_unique_bits_be()?;
        let modulus_bits = E::Fr::size_in_bits();
        let bits_to_skip = modulus_bits - self.required_bit_size as usize;
        for b in bits[..bits_to_skip].iter() {
            b.enforce_equal(&Boolean::constant(false))?;
        }
        bits[bits_to_skip].enforce_equal(&Boolean::constant(true))?;

        Ok(())
    }
}

pub struct Protocol<E: PairingEngine> {
    pub crs: CRSHashToPrime<E::G1Projective, Self>,
}

impl<E: PairingEngine> HashToPrimeProtocol<E::G1Projective> for Protocol<E> {
    type Proof = legogro16::Proof<E>;
    type Parameters = legogro16::ProvingKey<E>;

    fn from_crs(crs: &CRSHashToPrime<E::G1Projective, Self>) -> Protocol<E> {
        Protocol {
            crs: (*crs).clone(),
        }
    }

    fn setup<R: Rng>(
        rng: &mut R,
        pedersen_commitment_parameters: &PedersenCommitment<E::G1Projective>,
        parameters: &Parameters,
    ) -> Result<Self::Parameters, SetupError> {
        let c = HashToPrimeCircuit::<E> {
            required_bit_size: parameters.hash_to_prime_bits,
            value: None,
        };
        let base_one = E::G1Projective::rand(rng);
        let pedersen_bases = vec![
            base_one,
            pedersen_commitment_parameters.g,
            pedersen_commitment_parameters.h,
        ];
        Ok(legogro16::generate_random_parameters(
            c,
            &pedersen_bases
                .into_iter()
                .map(|p| p.into_affine())
                .collect::<Vec<_>>(),
            rng,
        )?)
    }

    fn prove<R: Rng, C: HashToPrimeVerifierChannel<E::G1Projective, Self>>(
        &self,
        verifier_channel: &mut C,
        rng: &mut R,
        _: &Statement<E::G1Projective>,
        witness: &Witness,
    ) -> Result<(), ProofError> {
        let c = HashToPrimeCircuit::<E> {
            required_bit_size: self.crs.parameters.hash_to_prime_bits,
            value: Some(integer_to_bigint_mod_q::<E::G1Projective>(
                &witness.e.clone(),
            )?),
        };
        let v = E::Fr::rand(rng);
        let link_v = integer_to_bigint_mod_q::<E::G1Projective>(&witness.r_q.clone())?;
        let proof = legogro16::create_random_proof::<E, _, _>(
            c,
            v,
            link_v,
            &self.crs.hash_to_prime_parameters,
            rng,
        )?;
        verifier_channel.send_proof(&proof)?;
        Ok(())
    }

    fn verify<C: HashToPrimeProverChannel<E::G1Projective, Self>>(
        &self,
        prover_channel: &mut C,
        statement: &Statement<E::G1Projective>,
    ) -> Result<(), VerificationError> {
        let proof = prover_channel.receive_proof()?;
        let pvk = legogro16::prepare_verifying_key(&self.crs.hash_to_prime_parameters.vk);
        if !legogro16::verify_proof(&pvk, &proof)? {
            return Err(VerificationError::VerificationFailed);
        }
        let proof_link_d_without_one = proof
            .link_d
            .into_projective()
            .sub(&self.crs.hash_to_prime_parameters.vk.link_bases[0].into_projective());
        if statement.c_e_q != proof_link_d_without_one {
            return Err(VerificationError::VerificationFailed);
        }

        Ok(())
    }

    fn hash_to_prime(&self, e: &Integer) -> Result<(Integer, u64), HashToPrimeError> {
        Ok((e.clone(), 0))
    }
}

#[cfg(test)]
mod test {
    use super::{HashToPrimeCircuit, Protocol, Statement, Witness};
    use crate::{
        commitments::Commitment,
        parameters::Parameters,
        protocols::hash_to_prime::{
            snark_range::Protocol as HPProtocol,
            transcript::{TranscriptProverChannel, TranscriptVerifierChannel},
            HashToPrimeProtocol,
        },
        utils::integer_to_bigint_mod_q,
    };
    use accumulator::group::Rsa2048;
    use ark_bls12_381::{Bls12_381, Fr, G1Projective};
    use ark_relations::r1cs::{ConstraintSynthesizer, ConstraintSystem};
    use merlin::Transcript;
    use rand::thread_rng;
    use rug::rand::RandState;
    use rug::Integer;
    use std::cell::RefCell;

    #[test]
    fn test_circuit() {
        let cs = ConstraintSystem::<Fr>::new_ref();
        let c = HashToPrimeCircuit::<Bls12_381> {
            required_bit_size: 4,
            value: Some(integer_to_bigint_mod_q::<G1Projective>(&Integer::from(12)).unwrap()),
        };
        c.generate_constraints(cs.clone()).unwrap();
        println!("num constraints: {}", cs.num_constraints());
        if !cs.is_satisfied().unwrap() {
            panic!("not satisfied: {:?}", cs.which_is_unsatisfied().unwrap());
        }
    }

    #[test]
    fn test_proof() {
        let params = Parameters::from_security_level(128).unwrap();
        let mut rng1 = RandState::new();
        rng1.seed(&Integer::from(13));
        let mut rng2 = thread_rng();

        let crs = crate::protocols::membership::Protocol::<
            Rsa2048,
            G1Projective,
            HPProtocol<Bls12_381>,
        >::setup(&params, &mut rng1, &mut rng2)
        .unwrap()
        .crs
        .crs_hash_to_prime;
        let protocol = Protocol::<Bls12_381>::from_crs(&crs);

        let value = Integer::from(Integer::u_pow_u(
            2,
            (crs.parameters.hash_to_prime_bits) as u32,
        )) - &Integer::from(245);
        let randomness = Integer::from(9);
        let commitment = protocol
            .crs
            .pedersen_commitment_parameters
            .commit(&value, &randomness)
            .unwrap();

        let proof_transcript = RefCell::new(Transcript::new(b"hash_to_prime"));
        let statement = Statement { c_e_q: commitment };
        let mut verifier_channel = TranscriptVerifierChannel::new(&crs, &proof_transcript);
        protocol
            .prove(
                &mut verifier_channel,
                &mut rng2,
                &statement,
                &Witness {
                    e: value,
                    r_q: randomness,
                },
            )
            .unwrap();

        let proof = verifier_channel.proof().unwrap();

        let verification_transcript = RefCell::new(Transcript::new(b"hash_to_prime"));
        let mut prover_channel =
            TranscriptProverChannel::new(&crs, &verification_transcript, &proof);
        protocol.verify(&mut prover_channel, &statement).unwrap();
    }
}
