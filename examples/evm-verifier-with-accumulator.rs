use ethereum_types::Address;
use foundry_evm::executor::{fork::MultiFork, Backend, ExecutorBuilder};
use halo2_curves::bn256::{Bn256, Fq, Fr, G1Affine};
use halo2_proofs::{
    dev::MockProver,
    plonk::{create_proof, keygen_pk, keygen_vk, verify_proof, Circuit, ProvingKey, VerifyingKey},
    poly::{
        commitment::{Params, ParamsProver},
        kzg::{
            commitment::{KZGCommitmentScheme, ParamsKZG},
            multiopen::{ProverGWC, VerifierGWC},
            strategy::AccumulatorStrategy,
        },
        VerificationStrategy,
    },
    transcript::{EncodedChallenge, TranscriptReadBuffer, TranscriptWriterBuffer},
};
use itertools::Itertools;
use plonk_verifier::{
    loader::{
        evm::{encode_calldata, EvmLoader, EvmTranscript},
        native::NativeLoader,
    },
    pcs::kzg::{Gwc19, KzgOnSameCurve},
    system::halo2::{compile, Config},
    util::transcript::TranscriptRead,
    verifier::{self, PlonkVerifier},
};
use rand::rngs::OsRng;
use std::{io::Cursor, rc::Rc};

const LIMBS: usize = 4;
const BITS: usize = 68;
type Plonk = verifier::Plonk<KzgOnSameCurve<Bn256, Gwc19<Bn256>, LIMBS, BITS>>;

mod application {
    use halo2_curves::bn256::Fr;
    use halo2_proofs::{
        circuit::{Layouter, SimpleFloorPlanner, Value},
        plonk::{Advice, Circuit, Column, ConstraintSystem, Error, Fixed, Instance},
        poly::Rotation,
    };
    use rand::RngCore;

    #[derive(Clone, Copy)]
    pub struct StandardPlonkConfig {
        a: Column<Advice>,
        b: Column<Advice>,
        c: Column<Advice>,
        q_a: Column<Fixed>,
        q_b: Column<Fixed>,
        q_c: Column<Fixed>,
        q_ab: Column<Fixed>,
        constant: Column<Fixed>,
        #[allow(dead_code)]
        instance: Column<Instance>,
    }

    impl StandardPlonkConfig {
        fn configure(meta: &mut ConstraintSystem<Fr>) -> Self {
            let [a, b, c] = [(); 3].map(|_| meta.advice_column());
            let [q_a, q_b, q_c, q_ab, constant] = [(); 5].map(|_| meta.fixed_column());
            let instance = meta.instance_column();

            [a, b, c].map(|column| meta.enable_equality(column));

            meta.create_gate(
                "q_a·a + q_b·b + q_c·c + q_ab·a·b + constant + instance = 0",
                |meta| {
                    let [a, b, c] =
                        [a, b, c].map(|column| meta.query_advice(column, Rotation::cur()));
                    let [q_a, q_b, q_c, q_ab, constant] = [q_a, q_b, q_c, q_ab, constant]
                        .map(|column| meta.query_fixed(column, Rotation::cur()));
                    let instance = meta.query_instance(instance, Rotation::cur());
                    Some(
                        q_a * a.clone()
                            + q_b * b.clone()
                            + q_c * c
                            + q_ab * a * b
                            + constant
                            + instance,
                    )
                },
            );

            StandardPlonkConfig {
                a,
                b,
                c,
                q_a,
                q_b,
                q_c,
                q_ab,
                constant,
                instance,
            }
        }
    }

    #[derive(Clone, Default)]
    pub struct StandardPlonk(Fr);

    impl StandardPlonk {
        pub fn rand<R: RngCore>(mut rng: R) -> Self {
            Self(Fr::from(rng.next_u32() as u64))
        }

        pub fn num_instance() -> Vec<usize> {
            vec![1]
        }

        pub fn instances(&self) -> Vec<Vec<Fr>> {
            vec![vec![self.0]]
        }
    }

    impl Circuit<Fr> for StandardPlonk {
        type Config = StandardPlonkConfig;
        type FloorPlanner = SimpleFloorPlanner;

        fn without_witnesses(&self) -> Self {
            Self::default()
        }

        fn configure(meta: &mut ConstraintSystem<Fr>) -> Self::Config {
            meta.set_minimum_degree(4);
            StandardPlonkConfig::configure(meta)
        }

        fn synthesize(
            &self,
            config: Self::Config,
            mut layouter: impl Layouter<Fr>,
        ) -> Result<(), Error> {
            layouter.assign_region(
                || "",
                |mut region| {
                    region.assign_advice(|| "", config.a, 0, || Value::known(self.0))?;
                    region.assign_fixed(|| "", config.q_a, 0, || Value::known(-Fr::one()))?;

                    region.assign_advice(|| "", config.a, 1, || Value::known(-Fr::from(5)))?;
                    for (idx, column) in (1..).zip([
                        config.q_a,
                        config.q_b,
                        config.q_c,
                        config.q_ab,
                        config.constant,
                    ]) {
                        region.assign_fixed(|| "", column, 1, || Value::known(Fr::from(idx)))?;
                    }

                    let a = region.assign_advice(|| "", config.a, 2, || Value::known(Fr::one()))?;
                    a.copy_advice(|| "", &mut region, config.b, 3)?;
                    a.copy_advice(|| "", &mut region, config.c, 4)?;

                    Ok(())
                },
            )
        }
    }
}

mod aggregation {
    use super::{Plonk, BITS, LIMBS};
    use halo2_curves::bn256::{Bn256, Fq, Fr, G1Affine};
    use halo2_proofs::{
        circuit::{Layouter, SimpleFloorPlanner, Value},
        plonk::{self, Circuit, ConstraintSystem},
        poly::{commitment::ParamsProver, kzg::commitment::ParamsKZG},
        transcript::TranscriptReadBuffer,
    };
    use halo2_wrong_ecc::{
        integer::rns::Rns,
        maingate::{
            MainGate, MainGateConfig, MainGateInstructions, RangeChip, RangeConfig,
            RangeInstructions, RegionCtx,
        },
        EccConfig,
    };
    use halo2_wrong_transcript::NativeRepresentation;
    use itertools::Itertools;
    use plonk_verifier::{
        loader::halo2,
        pcs::{
            kzg::{Accumulator, PreAccumulator},
            PreAccumulator as _,
        },
        util::{
            arithmetic::{fe_to_limbs, FieldExt},
            transcript::Transcript,
        },
        verifier::PlonkVerifier,
        Protocol,
    };
    use std::{iter, rc::Rc};

    const T: usize = 5;
    const RATE: usize = 4;
    const R_F: usize = 8;
    const R_P: usize = 57;

    type BaseFieldEccChip = halo2_wrong_ecc::BaseFieldEccChip<G1Affine, LIMBS, BITS>;
    type Halo2Loader<'a> = halo2::Halo2Loader<'a, G1Affine, Fr, BaseFieldEccChip>;
    pub type PoseidonTranscript<L, S, B> = halo2::PoseidonTranscript<
        G1Affine,
        Fr,
        NativeRepresentation,
        L,
        S,
        B,
        LIMBS,
        BITS,
        T,
        RATE,
        R_F,
        R_P,
    >;

    pub struct Snark {
        protocol: Protocol<G1Affine>,
        instances: Vec<Vec<Fr>>,
        proof: Vec<u8>,
    }

    impl Snark {
        pub fn new(protocol: Protocol<G1Affine>, instances: Vec<Vec<Fr>>, proof: Vec<u8>) -> Self {
            Self {
                protocol,
                instances,
                proof,
            }
        }
    }

    impl From<Snark> for SnarkWitness {
        fn from(snark: Snark) -> Self {
            Self {
                protocol: snark.protocol,
                instances: snark
                    .instances
                    .into_iter()
                    .map(|instances| instances.into_iter().map(Value::known).collect_vec())
                    .collect(),
                proof: Value::known(snark.proof),
            }
        }
    }

    #[derive(Clone)]
    pub struct SnarkWitness {
        protocol: Protocol<G1Affine>,
        instances: Vec<Vec<Value<Fr>>>,
        proof: Value<Vec<u8>>,
    }

    impl SnarkWitness {
        fn without_witnesses(&self) -> Self {
            SnarkWitness {
                protocol: self.protocol.clone(),
                instances: self
                    .instances
                    .iter()
                    .map(|instances| vec![Value::unknown(); instances.len()])
                    .collect(),
                proof: Value::unknown(),
            }
        }
    }

    pub fn accumulate<'a>(
        g1: &G1Affine,
        loader: &Rc<Halo2Loader<'a>>,
        snark: &SnarkWitness,
        curr_accumulator: Option<PreAccumulator<G1Affine, Rc<Halo2Loader<'a>>>>,
    ) -> PreAccumulator<G1Affine, Rc<Halo2Loader<'a>>> {
        let mut transcript = PoseidonTranscript::<Rc<Halo2Loader>, _, _>::new(
            loader,
            snark.proof.as_ref().map(|proof| proof.as_slice()),
        );
        let instances = snark
            .instances
            .iter()
            .map(|instances| {
                instances
                    .iter()
                    .map(|instance| loader.assign_scalar(*instance))
                    .collect_vec()
            })
            .collect_vec();
        let proof = Plonk::read_proof(&snark.protocol, &instances, &mut transcript).unwrap();
        let mut accumulator =
            Plonk::succint_verify(g1, &snark.protocol, &instances, &mut transcript, &proof)
                .unwrap();
        if let Some(curr_accumulator) = curr_accumulator {
            accumulator += curr_accumulator * transcript.squeeze_challenge();
        }
        accumulator
    }

    #[derive(Clone)]
    pub struct AggregationConfig {
        main_gate_config: MainGateConfig,
        range_config: RangeConfig,
    }

    impl AggregationConfig {
        pub fn configure<F: FieldExt>(
            meta: &mut ConstraintSystem<F>,
            composition_bits: Vec<usize>,
            overflow_bits: Vec<usize>,
        ) -> Self {
            let main_gate_config = MainGate::<F>::configure(meta);
            let range_config =
                RangeChip::<F>::configure(meta, &main_gate_config, composition_bits, overflow_bits);
            AggregationConfig {
                main_gate_config,
                range_config,
            }
        }

        pub fn main_gate(&self) -> MainGate<Fr> {
            MainGate::new(self.main_gate_config.clone())
        }

        pub fn range_chip(&self) -> RangeChip<Fr> {
            RangeChip::new(self.range_config.clone())
        }

        pub fn ecc_chip(&self) -> BaseFieldEccChip {
            BaseFieldEccChip::new(EccConfig::new(
                self.range_config.clone(),
                self.main_gate_config.clone(),
            ))
        }
    }

    #[derive(Clone)]
    pub struct AggregationCircuit {
        g1: G1Affine,
        snarks: Vec<SnarkWitness>,
        instances: Vec<Fr>,
    }

    impl AggregationCircuit {
        pub fn new(params: &ParamsKZG<Bn256>, snarks: impl IntoIterator<Item = Snark>) -> Self {
            let g1 = params.get_g()[0];
            let snarks = snarks.into_iter().collect_vec();

            let accumulator = snarks
                .iter()
                .fold(None, |curr_accumulator, snark| {
                    let mut transcript = PoseidonTranscript::init(snark.proof.as_slice());
                    let proof =
                        Plonk::read_proof(&snark.protocol, &snark.instances, &mut transcript)
                            .unwrap();
                    let mut accumulator = Plonk::succint_verify(
                        &g1,
                        &snark.protocol,
                        &snark.instances,
                        &mut transcript,
                        &proof,
                    )
                    .unwrap();
                    if let Some(curr_accumulator) = curr_accumulator {
                        accumulator += curr_accumulator * transcript.squeeze_challenge();
                    }
                    Some(accumulator)
                })
                .unwrap();

            let Accumulator { lhs, rhs } = accumulator.evaluate();
            let instances = [lhs.x, lhs.y, rhs.x, rhs.y]
                .map(fe_to_limbs::<_, _, LIMBS, BITS>)
                .concat();

            Self {
                g1,
                snarks: snarks.into_iter().map_into().collect(),
                instances,
            }
        }

        pub fn accumulator_indices() -> Vec<(usize, usize)> {
            (0..4 * LIMBS).map(|idx| (0, idx)).collect()
        }

        pub fn num_instance() -> Vec<usize> {
            vec![4 * LIMBS]
        }

        pub fn instances(&self) -> Vec<Vec<Fr>> {
            vec![self.instances.clone()]
        }
    }

    impl Circuit<Fr> for AggregationCircuit {
        type Config = AggregationConfig;
        type FloorPlanner = SimpleFloorPlanner;

        fn without_witnesses(&self) -> Self {
            Self {
                g1: self.g1,
                snarks: self
                    .snarks
                    .iter()
                    .map(SnarkWitness::without_witnesses)
                    .collect(),
                instances: Vec::new(),
            }
        }

        fn configure(meta: &mut plonk::ConstraintSystem<Fr>) -> Self::Config {
            AggregationConfig::configure(
                meta,
                vec![BITS / LIMBS],
                Rns::<Fq, Fr, LIMBS, BITS>::construct().overflow_lengths(),
            )
        }

        fn synthesize(
            &self,
            config: Self::Config,
            mut layouter: impl Layouter<Fr>,
        ) -> Result<(), plonk::Error> {
            let main_gate = config.main_gate();
            let range_chip = config.range_chip();

            range_chip.load_table(&mut layouter)?;

            let (lhs, rhs) = layouter.assign_region(
                || "",
                |region| {
                    let ctx = RegionCtx::new(region, 0);

                    let ecc_chip = config.ecc_chip();
                    let loader = Halo2Loader::new(ecc_chip, ctx);
                    let accumulator = self
                        .snarks
                        .iter()
                        .fold(None, |accumulator, snark| {
                            Some(accumulate(&self.g1, &loader, snark, accumulator))
                        })
                        .unwrap();
                    let Accumulator { lhs, rhs } = accumulator.evaluate();

                    Ok((lhs.into_normalized(), rhs.into_normalized()))
                },
            )?;

            for (limb, row) in iter::empty()
                .chain(lhs.x().limbs())
                .chain(lhs.y().limbs())
                .chain(rhs.x().limbs())
                .chain(rhs.y().limbs())
                .zip(0..)
            {
                main_gate.expose_public(layouter.namespace(|| ""), limb.into(), row)?;
            }

            Ok(())
        }
    }
}

fn gen_srs(k: u32) -> ParamsKZG<Bn256> {
    ParamsKZG::<Bn256>::setup(k, OsRng)
}

fn gen_pk<C: Circuit<Fr>>(params: &ParamsKZG<Bn256>, circuit: &C) -> ProvingKey<G1Affine> {
    let vk = keygen_vk(params, circuit).unwrap();
    keygen_pk(params, vk, circuit).unwrap()
}

fn gen_proof<
    C: Circuit<Fr>,
    E: EncodedChallenge<G1Affine>,
    TR: TranscriptReadBuffer<Cursor<Vec<u8>>, G1Affine, E>,
    TW: TranscriptWriterBuffer<Vec<u8>, G1Affine, E>,
>(
    params: &ParamsKZG<Bn256>,
    pk: &ProvingKey<G1Affine>,
    circuit: C,
    instances: Vec<Vec<Fr>>,
) -> Vec<u8> {
    MockProver::run(params.k(), &circuit, instances.clone())
        .unwrap()
        .assert_satisfied();

    let instances = instances
        .iter()
        .map(|instances| instances.as_slice())
        .collect_vec();
    let proof = {
        let mut transcript = TW::init(Vec::new());
        create_proof::<KZGCommitmentScheme<Bn256>, ProverGWC<_>, _, _, TW, _>(
            params,
            pk,
            &[circuit],
            &[instances.as_slice()],
            OsRng,
            &mut transcript,
        )
        .unwrap();
        transcript.finalize()
    };

    let accept = {
        let mut transcript = TR::init(Cursor::new(proof.clone()));
        VerificationStrategy::<_, VerifierGWC<_>>::finalize(
            verify_proof::<_, VerifierGWC<_>, _, TR, _>(
                params.verifier_params(),
                pk.get_vk(),
                AccumulatorStrategy::new(params.verifier_params()),
                &[instances.as_slice()],
                &mut transcript,
            )
            .unwrap(),
        )
    };
    assert!(accept);

    proof
}

fn gen_application_snark(params: &ParamsKZG<Bn256>) -> aggregation::Snark {
    let circuit = application::StandardPlonk::rand(OsRng);

    let pk = gen_pk(params, &circuit);
    let protocol = compile(
        pk.get_vk(),
        Config {
            zk: true,
            query_instance: false,
            num_instance: application::StandardPlonk::num_instance(),
            num_proof: 1,
            accumulator_indices: None,
        },
    );

    let proof = gen_proof::<
        _,
        _,
        aggregation::PoseidonTranscript<NativeLoader, _, _>,
        aggregation::PoseidonTranscript<NativeLoader, _, _>,
    >(params, &pk, circuit.clone(), circuit.instances());
    aggregation::Snark::new(protocol, circuit.instances(), proof)
}

fn gen_aggregation_evm_verifier(
    params: &ParamsKZG<Bn256>,
    vk: &VerifyingKey<G1Affine>,
    num_instance: Vec<usize>,
    accumulator_indices: Vec<(usize, usize)>,
) -> Vec<u8> {
    let protocol = compile(
        vk,
        Config {
            zk: true,
            query_instance: false,
            num_instance: num_instance.clone(),
            num_proof: 1,
            accumulator_indices: Some(accumulator_indices),
        },
    );

    let loader = EvmLoader::new::<Fq, Fr>();
    let mut transcript = EvmTranscript::<_, Rc<EvmLoader>, _, _>::new(loader.clone());

    let instances = num_instance
        .into_iter()
        .map(|len| transcript.read_n_scalars(len).unwrap())
        .collect_vec();
    Plonk::verify(
        &params.get_g()[0],
        &(params.g2(), params.s_g2()),
        &protocol,
        &instances,
        &mut transcript,
    )
    .unwrap();

    loader.deployment_code()
}

fn evm_verify(deployment_code: Vec<u8>, instances: Vec<Vec<Fr>>, proof: Vec<u8>) {
    let calldata = encode_calldata(&instances, &proof);
    let success = {
        let mut evm = ExecutorBuilder::default()
            .with_gas_limit(u64::MAX.into())
            .build(Backend::new(MultiFork::new().0, None));

        let caller = Address::from_low_u64_be(0xfe);
        let verifier = evm
            .deploy(caller, deployment_code.into(), 0.into(), None)
            .unwrap()
            .address;
        let result = evm
            .call_raw(caller, verifier, calldata.into(), 0.into())
            .unwrap();

        dbg!(result.gas);

        !result.reverted
    };
    assert!(success);
}

fn main() {
    let params = gen_srs(21);
    let params_app = {
        let mut params = params.clone();
        params.downsize(8);
        params
    };

    let snarks = [(); 3].map(|_| gen_application_snark(&params_app));
    let agg_circuit = aggregation::AggregationCircuit::new(&params, snarks);
    let pk = gen_pk(&params, &agg_circuit);
    let deployment_code = gen_aggregation_evm_verifier(
        &params,
        pk.get_vk(),
        aggregation::AggregationCircuit::num_instance(),
        aggregation::AggregationCircuit::accumulator_indices(),
    );

    let proof = gen_proof::<_, _, EvmTranscript<G1Affine, _, _, _>, EvmTranscript<G1Affine, _, _, _>>(
        &params,
        &pk,
        agg_circuit.clone(),
        agg_circuit.instances(),
    );
    evm_verify(deployment_code, agg_circuit.instances(), proof);
}
