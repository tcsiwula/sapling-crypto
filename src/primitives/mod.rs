use pairing::{
    Field,
    PrimeField,
    PrimeFieldRepr
};

use constants;

use group_hash::group_hash;

use pedersen_hash::{
    pedersen_hash,
    Personalization
};

use byteorder::{
    BigEndian,
    WriteBytesExt
};

use jubjub::{
    JubjubEngine,
    JubjubParams,
    edwards,
    PrimeOrder,
    FixedGenerators
};

use blake2_rfc::blake2s::Blake2s;

#[derive(Clone)]
pub struct ValueCommitment<E: JubjubEngine> {
    pub value: u64,
    pub randomness: E::Fs
}

impl<E: JubjubEngine> ValueCommitment<E> {
    pub fn cm(
        &self,
        params: &E::Params
    ) -> edwards::Point<E, PrimeOrder>
    {
        params.generator(FixedGenerators::ValueCommitmentValue)
              .mul(self.value, params)
              .add(
                  &params.generator(FixedGenerators::ValueCommitmentRandomness)
                  .mul(self.randomness, params),
                  params
              )
    }
}

#[derive(Clone)]
pub struct ProofGenerationKey<E: JubjubEngine> {
    pub ak: edwards::Point<E, PrimeOrder>,
    pub rsk: E::Fs
}

impl<E: JubjubEngine> ProofGenerationKey<E> {
    pub fn into_viewing_key(&self, params: &E::Params) -> ViewingKey<E> {
        ViewingKey {
            ak: self.ak.clone(),
            rk: params.generator(FixedGenerators::ProofGenerationKey)
                      .mul(self.rsk, params)
        }
    }
}

pub struct ViewingKey<E: JubjubEngine> {
    pub ak: edwards::Point<E, PrimeOrder>,
    pub rk: edwards::Point<E, PrimeOrder>
}

impl<E: JubjubEngine> ViewingKey<E> {
    fn ivk(&self) -> E::Fs {
        let mut preimage = [0; 64];

        self.ak.write(&mut preimage[0..32]).unwrap();
        self.rk.write(&mut preimage[32..64]).unwrap();

        let mut h = Blake2s::with_params(32, &[], &[], constants::CRH_IVK_PERSONALIZATION);
        h.update(&preimage);
        let mut h = h.finalize().as_ref().to_vec();

        // Drop the first five bits, so it can be interpreted as a scalar.
        h[0] &= 0b0000_0111;

        let mut e = <E::Fs as PrimeField>::Repr::default();
        e.read_be(&h[..]).unwrap();

        E::Fs::from_repr(e).expect("should be a valid scalar")
    }

    pub fn into_payment_address(
        &self,
        diversifier: Diversifier,
        params: &E::Params
    ) -> Option<PaymentAddress<E>>
    {
        diversifier.g_d(params).map(|g_d| {
            let pk_d = g_d.mul(self.ivk(), params);

            PaymentAddress {
                pk_d: pk_d,
                diversifier: diversifier
            }
        })
    }
}

#[derive(Copy, Clone)]
pub struct Diversifier(pub [u8; 11]);

impl Diversifier {
    pub fn g_d<E: JubjubEngine>(
        &self,
        params: &E::Params
    ) -> Option<edwards::Point<E, PrimeOrder>>
    {
        group_hash::<E>(&self.0, constants::KEY_DIVERSIFICATION_PERSONALIZATION, params)
    }
}

#[derive(Clone)]
pub struct PaymentAddress<E: JubjubEngine> {
    pub pk_d: edwards::Point<E, PrimeOrder>,
    pub diversifier: Diversifier
}

impl<E: JubjubEngine> PaymentAddress<E> {
    pub fn g_d(
        &self,
        params: &E::Params
    ) -> Option<edwards::Point<E, PrimeOrder>>
    {
        self.diversifier.g_d(params)
    }

    pub fn create_note(
        &self,
        value: u64,
        randomness: E::Fs,
        params: &E::Params
    ) -> Option<Note<E>>
    {
        self.g_d(params).map(|g_d| {
            Note {
                value: value,
                r: randomness,
                g_d: g_d,
                pk_d: self.pk_d.clone()
            }
        })
    }
}

pub struct Note<E: JubjubEngine> {
    /// The value of the note
    pub value: u64,
    /// The diversified base of the address, GH(d)
    pub g_d: edwards::Point<E, PrimeOrder>,
    /// The public key of the address, g_d^ivk
    pub pk_d: edwards::Point<E, PrimeOrder>,
    /// The commitment randomness
    pub r: E::Fs
}

impl<E: JubjubEngine> Note<E> {
    pub fn uncommitted() -> E::Fr {
        // The smallest u-coordinate that is not on the curve
        // is one.
        // TODO: This should be relocated to JubjubEngine as
        // it's specific to the curve we're using, not all
        // twisted edwards curves.
        E::Fr::one()
    }

    /// Computes the note commitment, returning the full point.
    fn cm_full_point(&self, params: &E::Params) -> edwards::Point<E, PrimeOrder>
    {
        // Calculate the note contents, as bytes
        let mut note_contents = vec![];

        // Write the value in big endian
        (&mut note_contents).write_u64::<BigEndian>(self.value).unwrap();

        // Write g_d
        self.g_d.write(&mut note_contents).unwrap();

        // Write pk_d
        self.pk_d.write(&mut note_contents).unwrap();

        assert_eq!(note_contents.len(), 32 + 32 + 8);

        // Compute the Pedersen hash of the note contents
        let hash_of_contents = pedersen_hash(
            Personalization::NoteCommitment,
            note_contents.into_iter()
                         .flat_map(|byte| {
                            (0..8).rev().map(move |i| ((byte >> i) & 1) == 1)
                         }),
            params
        );

        // Compute final commitment
        params.generator(FixedGenerators::NoteCommitmentRandomness)
              .mul(self.r, params)
              .add(&hash_of_contents, params)
    }

    /// Computes the nullifier given the viewing key and
    /// note position
    pub fn nf(
        &self,
        viewing_key: &ViewingKey<E>,
        position: u64,
        params: &E::Params
    ) -> edwards::Point<E, PrimeOrder>
    {
        // Compute cm + position
        let cm_plus_position = self
            .cm_full_point(params)
            .add(
                &params.generator(FixedGenerators::NullifierPosition)
                       .mul(position, params),
                params
            );

        // Compute nr = drop_5(BLAKE2s(rk | cm_plus_position))
        let mut nr_preimage = [0u8; 64];
        viewing_key.rk.write(&mut nr_preimage[0..32]).unwrap();
        cm_plus_position.write(&mut nr_preimage[32..64]).unwrap();
        let mut h = Blake2s::with_params(32, &[], &[], constants::PRF_NR_PERSONALIZATION);
        h.update(&nr_preimage);
        let mut h = h.finalize().as_ref().to_vec();

        // Drop the first five bits, so it can be interpreted as a scalar.
        h[0] &= 0b0000_0111;

        let mut e = <E::Fs as PrimeField>::Repr::default();
        e.read_be(&h[..]).unwrap();

        let nr = E::Fs::from_repr(e).expect("should be a valid scalar");

        viewing_key.ak.mul(nr, params)
    }

    /// Computes the note commitment
    pub fn cm(&self, params: &E::Params) -> E::Fr
    {
        // The commitment is in the prime order subgroup, so mapping the
        // commitment to the x-coordinate is an injective encoding.
        self.cm_full_point(params).into_xy().0
    }
}
