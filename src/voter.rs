use core::{
    num::NonZeroUsize,
    task::{Poll, Waker},
    time::Duration,
};

use crate::std::sync::Arc;

use crate::std::collections::BTreeMap;
use futures::{Future, FutureExt, SinkExt, StreamExt};
use parking_lot::Mutex;
use tracing::{info, trace, Value};

use crate::{
    environment::{Environment, RoundData, VoterData},
    messages::{
        FinalizedCommit, Message, Precommit, Prevote, Proposal, SignedCommit, SignedMessage,
    },
    VoterSet,
};

enum CurrentState {
    Proposal,
    Prevote,
    Precommit,
}

impl CurrentState {
    pub fn new() -> Self {
        CurrentState::Proposal
    }
}

pub struct Voter<E: Environment> {
    env: Arc<E>,
    global: Arc<Mutex<GlobalState<E>>>,
    global_in: E::GlobalIn,
    global_out: E::GlobalOut,
}

impl<E: Environment> Voter<E> {
    fn new(env: Arc<E>) -> Self {
        let VoterData {
            finalized_target,
            global_in,
            global_out,
            local_id,
            voters,
        } = env.init_voter();
        let global = Arc::new(Mutex::new(GlobalState::new(local_id, voters)));
        global.lock().set_finalized_target(finalized_target);
        Voter {
            env,
            global_in,
            global_out,
            global,
        }
    }

    async fn start(&mut self) {
        loop {
            let round = self.global.lock().round;

            let voting_round = Round::new(self.env.clone(), round, self.global.clone());

            let round_state = voting_round.round_state.clone();
            let incoming = async move {
                let mut incoming = round_state.lock().incoming.take().unwrap();
                while let Some(Ok(signed_msg)) = incoming.next().await {
                    round_state.lock().process_incoming(signed_msg);
                }
            };

            tokio::select! {
                _ = incoming => {},
                res = voting_round.run() => {
                    match res {
                        Ok(f_commit) => {
                            // Send commit to global_out;
                            self.env.finalize_block(
                                round,
                                f_commit.target_hash.clone(),
                                f_commit.target_number.clone(),
                                f_commit,
                            );
                        }
                        Err(provotes) => {
                            // save round data to global state.
                            self.global.lock().append_round(round, provotes)
                        }
                    }
                },
            }
        }
    }
}

pub struct Round<E: Environment> {
    local_id: E::Id,
    env: Arc<E>,
    outgoing: E::Out,
    round_state: Arc<Mutex<RoundState<E>>>,
}

impl<E: Environment> Round<E> {
    fn new(env: Arc<E>, round: u64, global: Arc<Mutex<GlobalState<E>>>) -> Self {
        let RoundData {
            local_id,
            incoming,
            outgoing,
            ..
        } = env.init_round(round);
        let proposer = global.lock().voters.get_proposer(round);
        let round_state = Arc::new(Mutex::new(RoundState::new(incoming, proposer, global)));
        Round {
            env,
            outgoing,
            local_id,
            round_state,
        }
    }

    fn valid_prevotes(
        &self,
        prevotes: Vec<Prevote<E::Number, E::Hash>>,
    ) -> Prevote<E::Number, E::Hash> {
        prevotes.first().unwrap().clone()
    }

    fn valid_precommits(
        &self,
        precommits: Vec<SignedCommit<E::Number, E::Hash, E::Signature, E::Id>>,
    ) -> Precommit<E::Number, E::Hash> {
        precommits.first().unwrap().commit.clone()
    }

    async fn run(
        mut self,
    ) -> Result<
        FinalizedCommit<E::Number, E::Hash, E::Signature, E::Id>,
        Vec<Prevote<E::Number, E::Hash>>,
    > {
        // TODO: maybe eliminate this sleep.
        tokio::time::sleep(Duration::from_millis(1000)).await;
        let global = self.round_state.lock().global.clone();

        let height = global.lock().height;
        let round = global.lock().round;
        // if I'm the proposer
        let is_proposer = self.round_state.lock().is_proposer();
        info!(
            id = self.local_id,
            "Round {}: proposer {}", round, is_proposer
        );
        if is_proposer {
            // broadcast proposal
            let valid_value = global.lock().valid_value.clone();
            if let Some(vv) = valid_value {
                info!(id = self.local_id, "valid_value: {:?}", vv);
                let valid_round = global.lock().valid_round;
                let proposal = Message::Proposal(Proposal {
                    target_hash: vv,
                    target_height: height + num::one(),
                    valid_round,
                    round,
                });
                info!(id = self.local_id, "Proposing {:?}", proposal);
                self.outgoing.send(proposal).await;
            } else {
                info!(id = self.local_id, "No valid value");
                let decision = global.lock().decision.clone();
                info!(
                    id = self.local_id,
                    "decision: {:?}, height: {:?}", decision, height
                );

                let finalized_hash = decision.get(&height).unwrap().clone();

                // let finalized_hash = global.lock().decision.get(&height).unwrap().clone();
                info!(id = self.local_id, "current_target {:?}", finalized_hash);
                let (target_height, target_hash) = self
                    .env
                    .propose(round, finalized_hash)
                    .await
                    .unwrap()
                    .unwrap();
                // TODO: print panic stacktrace in console
                assert_eq!(target_height, height + num::one());
                if target_height == height {
                    // let it fall
                } else {
                    let proposal = Message::Proposal(Proposal {
                        target_hash,
                        target_height,
                        valid_round: None,
                        round,
                    });

                    info!(id = self.local_id, "Proposing {:?}", proposal);

                    self.outgoing.send(proposal).await;
                };
            }
        }

        let timeout = tokio::time::sleep(Duration::from_millis(1000));
        tokio::pin!(timeout);
        let fu = futures::future::poll_fn(|cx| {
            let mut round_lock = self.round_state.lock();
            let proposal = &round_lock.proposal;
            if let Some(proposal) = &proposal {
                Poll::Ready(Ok(proposal.clone()))
            } else {
                round_lock.waker = Some(cx.waker().clone());
                timeout.poll_unpin(cx).map(|_| Err(()))
            }
        });

        info!(id = self.local_id, "Waiting for proposal");
        let provote = if let Ok(proposal) = fu.await {
            info!(id = self.local_id, "Got proposal {:?}", proposal);
            if let Some(vr) = proposal.valid_round {
                if vr < round && global.lock().get_round(vr).is_some() {
                    Message::Prevote(Prevote {
                        target_hash: Some(proposal.target_hash.clone()),
                        target_height: proposal.target_height,
                        round: proposal.round,
                    })
                } else {
                    Message::Prevote(Prevote {
                        target_hash: None,
                        target_height: proposal.target_height,
                        round: proposal.round,
                    })
                }
                // need find prevotes for vr
            } else {
                // no need
                // valid(v) ∧ (lockedRoundp = −1 ∨ lockedV aluep = v)
                let locked_round = global.lock().locked_round;
                let locked_value = global.lock().locked_value.clone();

                let proposal_target_hash = proposal.target_hash.clone();

                if locked_round == None || locked_value == Some(proposal_target_hash.clone()) {
                    Message::Prevote(Prevote {
                        target_hash: Some(proposal_target_hash),
                        target_height: proposal.target_height,
                        round: proposal.round,
                    })
                } else {
                    Message::Prevote(Prevote {
                        target_hash: None,
                        target_height: proposal.target_height,
                        round: proposal.round,
                    })
                }
            }
        } else {
            info!(id = self.local_id, "No proposal");
            // broadcast nil
            let target_height = global.lock().height;
            let round = global.lock().round;
            Message::Prevote(Prevote {
                target_hash: None,
                target_height,
                round,
            })
        };

        info!(id = self.local_id, "Sending provote {:?}", provote);
        self.outgoing.send(provote).await;

        global.lock().current_state = CurrentState::Prevote;

        let timeout = tokio::time::sleep(Duration::from_secs(1));
        tokio::pin!(timeout);
        let fu = futures::future::poll_fn(|cx| {
            let mut round_lock = self.round_state.lock();
            let provotes = &round_lock.prevotes;
            let threshold = global.lock().voters.threshold();
            if provotes.len() >= threshold {
                Poll::Ready(Ok(provotes.clone()))
            } else {
                round_lock.waker = Some(cx.waker().clone());
                timeout.poll_unpin(cx).map(|_| Err(()))
            }
        });

        let precommit = if let Ok(prevotes) = fu.await {
            info!(id = self.local_id, "Got prevotes {:?}", prevotes);
            global.lock().locked_value = None;
            let locked_round = global.lock().round;
            global.lock().locked_round = Some(locked_round);

            let prevote = self.valid_prevotes(prevotes);

            Message::Precommit(Precommit {
                target_hash: prevote.target_hash,
                target_height: prevote.target_height,
                round,
            })
        } else {
            Message::Precommit(Precommit {
                target_hash: None,
                target_height: height,
                round,
            })
        };

        info!(id = self.local_id, "Sending precommit {:?}", precommit);
        self.outgoing.send(precommit).await;
        // 37: if stepp = prevote then
        // 38: lockedV aluep ← v
        // 39: lockedRoundp ← roundp
        // 40: broadcast 〈PRECOMMIT, hp, roundp, id(v))〉
        // 41: stepp ← precommit
        // 42: validV aluep ← v
        // 43: validRoundp ← roundp
        let timeout = tokio::time::sleep(Duration::from_secs(1));
        tokio::pin!(timeout);
        let fu = futures::future::poll_fn(|cx| {
            let mut round_lock = self.round_state.lock();
            let precommits = &round_lock.precommits;
            if &precommits.len() >= &global.lock().voters.threshold() {
                Poll::Ready(Ok(precommits.clone()))
            } else {
                round_lock.waker = Some(cx.waker().clone());
                timeout.poll_unpin(cx).map(|_| Err(()))
            }
        });

        if let Ok(commits) = fu.await {
            info!(id = self.local_id, "Got precommits {:?}", commits);
            let commit = self.valid_precommits(commits.clone());

            if let Some(hash) = commit.target_hash {
                global
                    .lock()
                    .decision
                    .insert(commit.target_height, hash.clone());
                let new_height = global.lock().height + num::one();
                global.lock().height = new_height;
                global.lock().locked_value = None;
                global.lock().locked_round = None;
                global.lock().valid_value = None;
                global.lock().valid_round = None;

                let f_commit = FinalizedCommit {
                    commits,
                    target_hash: hash,
                    target_number: commit.target_height,
                };
                info!(id = self.local_id, "Finalize commit {:?}", f_commit);
                Ok(f_commit)
            } else {
                Err(self.round_state.lock().prevotes.clone())
            }
        } else {
            Err(self.round_state.lock().prevotes.clone())
        }
    }
}

pub struct GlobalState<E: Environment> {
    local_id: E::Id,
    height: E::Number,
    round: u64,
    decision: BTreeMap<E::Number, E::Hash>,
    locked_value: Option<E::Hash>,
    locked_round: Option<u64>,
    valid_value: Option<E::Hash>,
    valid_round: Option<u64>,
    voters: VoterSet<E::Id>,
    current_state: CurrentState,
    message_log: BTreeMap<u64, Vec<Prevote<E::Number, E::Hash>>>,
}

impl<E: Environment> GlobalState<E> {
    pub fn new(local_id: E::Id, voters: VoterSet<E::Id>) -> Self {
        GlobalState {
            local_id,
            height: num::zero(),
            round: num::zero(),
            decision: BTreeMap::new(),
            locked_value: None,
            locked_round: None,
            valid_value: None,
            valid_round: None,
            voters,
            current_state: CurrentState::Proposal,
            message_log: BTreeMap::new(),
        }
    }

    pub fn append_round(&mut self, round: u64, prevotes: Vec<Prevote<E::Number, E::Hash>>) {
        self.message_log.insert(round, prevotes);
    }

    pub fn get_round(&self, round: u64) -> Option<Vec<Prevote<E::Number, E::Hash>>> {
        self.message_log
            .get(&round)
            .cloned()
            .filter(|v| v.len() > self.voters.threshold())
    }

    pub fn set_finalized_target(&mut self, target: (E::Number, E::Hash)) {
        self.decision.insert(target.0, target.1);
        self.height = target.0;
    }
}

pub struct RoundState<E: Environment> {
    global: Arc<Mutex<GlobalState<E>>>,
    proposer: E::Id,
    proposal: Option<Proposal<E::Number, E::Hash>>,
    prevotes: Vec<Prevote<E::Number, E::Hash>>,
    precommits: Vec<SignedCommit<E::Number, E::Hash, E::Signature, E::Id>>,
    incoming: Option<E::In>,
    waker: Option<Waker>,
}

impl<E: Environment> RoundState<E> {
    fn new(incoming: E::In, proposer: E::Id, global: Arc<Mutex<GlobalState<E>>>) -> Self {
        RoundState {
            incoming: Some(incoming),
            proposal: None,
            prevotes: Vec::new(),
            precommits: Vec::new(),
            proposer,
            global,
            waker: None,
        }
    }

    fn is_proposer(&self) -> bool {
        self.proposer == self.global.lock().local_id
    }

    fn process_incoming(
        &mut self,
        signed_msg: SignedMessage<
            <E as Environment>::Number,
            <E as Environment>::Hash,
            <E as Environment>::Signature,
            <E as Environment>::Id,
        >,
    ) {
        let SignedMessage {
            id,
            message: msg,
            signature,
        } = signed_msg;
        match msg {
            Message::Proposal(proposal) => {
                if self.proposer == id {
                    self.proposal = Some(proposal);
                }
            }
            Message::Prevote(prevote) => {
                self.prevotes.push(prevote);
            }
            Message::Precommit(precommit) => {
                self.precommits.push(SignedCommit {
                    commit: precommit,
                    signature,
                    id,
                });
            }
        }
        self.waker.take().map(|w| w.wake());
    }
}

#[cfg(test)]
mod test {
    use futures::{executor::LocalPool, task::SpawnExt, StreamExt};
    #[cfg(feature = "deadlock_detection")]
    use parking_lot::deadlock;
    use tracing::{error, info};

    use crate::testing::GENESIS_HASH;
    use std::sync::Arc;

    use crate::testing::{environment::DummyEnvironment, network::make_network};

    use super::*;

    #[cfg(deadlock_detection)]
    async fn deadlock_detection() {
        loop {
            tokio::time::sleep(Duration::from_secs(10)).await;
            let deadlocks = deadlock::check_deadlock();
            if deadlocks.is_empty() {
                trace!("No deadlocks detected");
                continue;
            }

            error!("{} deadlocks detected", deadlocks.len());
            for (i, threads) in deadlocks.iter().enumerate() {
                error!("Deadlock #{}", i);
                for t in threads {
                    error!("Thread Id {:#?}", t.thread_id());
                    error!("{:#?}", t.backtrace());
                }
            }
        }
    }

    use std::sync::Once;
    static INIT: Once = Once::new();
    fn init() {
        INIT.call_once(|| {
            let subscriber = tracing_subscriber::fmt()
                .with_max_level(tracing::Level::INFO)
                .finish();

            tracing::subscriber::set_global_default(subscriber)
                .map_err(|_err| eprintln!("Unable to set global default subscriber"));

            #[cfg(feature = "deadlock_detection")]
            {
                info!("deadlock_detection is enabled");
                tokio::spawn(deadlock_detection());
            }
        });
    }

    #[tokio::test]
    async fn basic_test() {
        init();

        let local_id = 5;
        let voter_set = Arc::new(Mutex::new(VoterSet::new(vec![5]).unwrap()));

        let (network, routing_network) = make_network();

        let env = Arc::new(DummyEnvironment::new(network, local_id, voter_set));

        // init chain
        let _last_finalized = env.with_chain(|chain| {
            chain.push_blocks(GENESIS_HASH, &["A", "B", "C", "D", "E"]);
            log::trace!(
                "chain: {:?}, last_finalized: {:?}, next_to_be_finalized: {:?}",
                chain,
                chain.last_finalized(),
                chain.next_to_be_finalized()
            );
            chain.last_finalized()
        });

        let mut voter = Voter::new(env.clone());

        tokio::spawn(routing_network);

        tokio::spawn(async move {
            voter.start().await;
        });

        // run voter in background. scheduling it to shut down at the end.
        let finalized = env.finalized_stream();

        // wait for the best block to finalized.
        finalized
            .take_while(|&(_, n)| {
                log::info!("n: {}", n);
                futures::future::ready(n < 5)
            })
            .for_each(|v| {
                log::info!("v: {:?}", v);
                futures::future::ready(())
            })
            .await
    }

    #[tokio::test]
    async fn consensus_test() {
        init();
        let voters_num = 4;

        let voter_set = Arc::new(Mutex::new(
            VoterSet::new((0..voters_num).into_iter().collect()).unwrap(),
        ));

        let (network, routing_network) = make_network();

        let finalized_stream = (0..voters_num)
            .map(|local_id| {
                let env = Arc::new(DummyEnvironment::new(
                    network.clone(),
                    local_id,
                    voter_set.clone(),
                ));

                // init chain
                let _last_finalized = env.with_chain(|chain| {
                    chain.push_blocks(GENESIS_HASH, &["A", "B", "C", "D", "E"]);
                    log::trace!(
                        "chain: {:?}, last_finalized: {:?}, next_to_be_finalized: {:?}",
                        chain,
                        chain.last_finalized(),
                        chain.next_to_be_finalized()
                    );
                    chain.last_finalized()
                });

                let mut voter = Voter::new(env.clone());

                tokio::spawn(async move {
                    voter.start().await;
                });

                // run voter in background. scheduling it to shut down at the end.
                let finalized = env.finalized_stream();

                // wait for the best block to finalized.
                finalized
                    .take_while(|&(_, n)| {
                        log::info!("n: {}", n);
                        futures::future::ready(n < 5)
                    })
                    .for_each(|v| {
                        log::info!("v: {:?}", v);
                        futures::future::ready(())
                    })
            })
            .collect::<Vec<_>>();

        tokio::spawn(routing_network);

        futures::future::join_all(finalized_stream.into_iter()).await;
    }

    #[tokio::test]
    async fn consensus_with_failed_node() {
        init();
        let voters_num = 4;
        let online_voters_num = 3;

        let default_panic = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            default_panic(info);
            std::process::exit(1);
        }));

        let voter_set = Arc::new(Mutex::new(
            VoterSet::new((0..voters_num).into_iter().collect()).unwrap(),
        ));

        let (network, routing_network) = make_network();

        let finalized_stream = (0..online_voters_num)
            .map(|local_id| {
                let env = Arc::new(DummyEnvironment::new(
                    network.clone(),
                    local_id,
                    voter_set.clone(),
                ));

                // init chain
                let _last_finalized = env.with_chain(|chain| {
                    chain.push_blocks(GENESIS_HASH, &["A", "B", "C", "D", "E"]);
                    log::trace!(
                        "chain: {:?}, last_finalized: {:?}, next_to_be_finalized: {:?}",
                        chain,
                        chain.last_finalized(),
                        chain.next_to_be_finalized()
                    );
                    chain.last_finalized()
                });

                let mut voter = Voter::new(env.clone());

                tokio::spawn(async move {
                    voter.start().await;
                });

                // run voter in background. scheduling it to shut down at the end.
                let finalized = env.finalized_stream();

                // wait for the best block to finalized.
                finalized
                    .take_while(|&(_, n)| {
                        log::info!("n: {}", n);
                        futures::future::ready(n < 5)
                    })
                    .for_each(|v| {
                        log::info!("v: {:?}", v);
                        futures::future::ready(())
                    })
            })
            .collect::<Vec<_>>();

        tokio::spawn(routing_network);

        futures::future::join_all(finalized_stream.into_iter()).await;
    }
}
