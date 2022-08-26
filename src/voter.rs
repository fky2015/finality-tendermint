use core::{num::NonZeroUsize, task::Poll, time::Duration};

use std::sync::Arc;

use futures::{Future, FutureExt, SinkExt, StreamExt};
use parking_lot::Mutex;
use std::collections::BTreeMap;
use tracing::info;

use crate::{
    environment::{Environment, RoundData, VoterData},
    messages::{
        FinalizedCommit, Message, Precommit, Prevote, Proposal, SignedCommit, SignedMessage,
    },
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

/// A set of nodes valid to vote.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct VoterSet<Id: Eq + Ord> {
    /// Voter's Id, with the same order as the vector in genesis block (or the following).
    voters: Vec<Id>,
    /// The required threshold number for supermajority.
    /// Normally, it's > 2/3.
    threshold: usize,
}

impl<Id: Eq + Ord + Clone> VoterSet<Id> {
    pub fn new(voters: Vec<Id>) -> Option<Self> {
        if voters.is_empty() {
            None
        } else {
            let len = voters.len() - (voters.len() - 1) / 3;
            Some(Self {
                voters,
                threshold: len,
            })
        }
    }

    pub fn add(&mut self, id: Id) {
        self.voters.push(id);
    }

    pub fn remove(&mut self, id: &Id) {
        self.voters.retain(|x| x != id);
    }

    pub fn is_empty(&self) -> bool {
        self.voters.is_empty()
    }

    pub fn is_full(&self) -> bool {
        self.voters.len() >= self.threshold
    }

    pub fn is_member(&self, id: &Id) -> bool {
        self.voters.contains(id)
    }

    pub fn threshold(&self) -> usize {
        self.threshold
    }

    /// Get the size of the set.
    pub fn len(&self) -> NonZeroUsize {
        unsafe {
            // SAFETY: By VoterSet::new()
            NonZeroUsize::new_unchecked(self.voters.len())
        }
    }

    /// Get the nth voter in the set, if any.
    ///
    /// Returns `None` if `n >= len`.
    pub fn nth(&self, n: usize) -> Option<&Id> {
        self.voters.get(n)
    }

    /// Get a ref to voters.
    pub fn voters(&self) -> &[Id] {
        &self.voters
    }

    /// Get leader Id.
    pub fn get_proposer(&self, round: u64) -> Id {
        self.voters
            .get(round as usize % self.voters.len())
            .cloned()
            .unwrap()
    }

    /// Whether the set contains a voter with the given ID.
    pub fn contains(&self, id: &Id) -> bool {
        self.voters.contains(id)
    }

    /// Get an iterator over the voters in the set, as given by
    /// the associated total order.
    pub fn iter(&self) -> impl Iterator<Item = &Id> {
        self.voters.iter()
    }

    /// Get the voter info for the voter with the given ID, if any.
    pub fn get(&self, id: &Id) -> Option<&Id> {
        if let Some(pos) = self.voters.iter().position(|i| id == i) {
            self.voters.get(pos)
        } else {
            None
        }
    }
}

// trait VoterT {
//     type E: Environment;
//     fn new(env: Self::E) -> Self;
//     async fn start();
// }

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
            info!("Voting round {}", round);

            let mut voting_round = Round::new(self.env.clone(), round, self.global.clone());

            let round_state = voting_round.round_state.clone();
            let mut incoming = async move {
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
        let global = self.round_state.lock().global.clone();

        let height = global.lock().height;
        let round = global.lock().round;
        // if I'm the proposer
        let is_proposer = self.round_state.lock().is_proposer();
        if is_proposer {
            // broadcast proposal
            let valid_value = global.lock().valid_value.clone();
            if let Some(vv) = valid_value {
                let valid_round = global.lock().valid_round;
                let proposal = Message::Proposal(Proposal {
                    target_hash: vv,
                    target_height: height + num::one(),
                    valid_round,
                    round,
                });
                self.outgoing.send(proposal).await;
            } else {
                let finalized_hash = global.lock().decision.get(&height).unwrap().clone();
                let (target_height, target_hash) = self
                    .env
                    .propose(round, finalized_hash)
                    .await
                    .unwrap()
                    .unwrap();
                if target_height == height {
                    // let it fall
                } else {
                    let proposal = Message::Proposal(Proposal {
                        target_hash,
                        target_height,
                        valid_round: None,
                        round,
                    });

                    info!("Proposing {:?}", proposal);

                    self.outgoing.send(proposal).await;
                };
            }
        }

        let timeout = tokio::time::sleep(Duration::from_millis(1000));
        tokio::pin!(timeout);
        let fu = futures::future::poll_fn(|cx| {
            if let Some(proposal) = &self.round_state.lock().proposal {
                Poll::Ready(Ok(proposal.clone()))
            } else {
                timeout.poll_unpin(cx).map(|_| Err(()))
            }
        });

        info!("Waiting for proposal");
        if let Ok(proposal) = fu.await {
            info!("Got proposal {:?}", proposal);
            if let Some(vr) = proposal.valid_round {
                if vr < round && global.lock().get_round(vr).is_some() {
                    let provote = Message::Prevote(Prevote {
                        target_hash: Some(proposal.target_hash.clone()),
                        target_height: proposal.target_height,
                        round: proposal.round,
                    });
                    self.outgoing.send(provote).await;
                } else {
                    let provote = Message::Prevote(Prevote {
                        target_hash: None,
                        target_height: proposal.target_height,
                        round: proposal.round,
                    });
                    self.outgoing.send(provote).await;
                }
                // need find prevotes for vr
            } else {
                // no need
                // valid(v) ∧ (lockedRoundp = −1 ∨ lockedV aluep = v)
                let locked_round = global.lock().locked_round;
                let locked_value = global.lock().locked_value.clone();

                let proposal_target_hash = proposal.target_hash.clone();

                if locked_round == None || locked_value == Some(proposal_target_hash.clone()) {
                    let provote = Message::Prevote(Prevote {
                        target_hash: Some(proposal_target_hash),
                        target_height: proposal.target_height,
                        round: proposal.round,
                    });
                    self.outgoing.send(provote).await;
                } else {
                    let provote = Message::Prevote(Prevote {
                        target_hash: None,
                        target_height: proposal.target_height,
                        round: proposal.round,
                    });
                    self.outgoing.send(provote).await;
                }
            }
        } else {
            info!("No proposal");
            // broadcast nil
            let target_height = global.lock().height;
            let round = global.lock().round;
            let provote = Message::Prevote(Prevote {
                target_hash: None,
                target_height,
                round,
            });
            self.outgoing.send(provote).await;
        }

        global.lock().current_state = CurrentState::Prevote;

        let timeout = tokio::time::sleep(Duration::from_secs(1));
        tokio::pin!(timeout);
        // TODO: fu.await
        let fu = futures::future::poll_fn(|cx| {
            // WARN: dead lock
            if &self.round_state.lock().prevotes.len() >= &global.lock().voters.threshold() {
                Poll::Ready(Ok(self.round_state.lock().prevotes.clone()))
            } else {
                match timeout.poll_unpin(cx) {
                    Poll::Ready(_) => Poll::Ready(Err(())),
                    Poll::Pending => Poll::Pending,
                }
            }
        });

        if let Ok(prevotes) = fu.await {
            global.lock().locked_value = None;
            global.lock().locked_round = Some(global.lock().round);

            let prevote = self.valid_prevotes(prevotes);

            let precommit = Message::Precommit(Precommit {
                target_hash: prevote.target_hash,
                target_height: prevote.target_height,
                round,
            });
            self.outgoing.send(precommit).await;
        } else {
            let precommit = Message::Precommit(Precommit {
                target_hash: None,
                target_height: height,
                round,
            });
            self.outgoing.send(precommit).await;
        }
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
            if &self.round_state.lock().precommits.len() >= &global.lock().voters.threshold() {
                Poll::Ready(Ok(self.round_state.lock().precommits.clone()))
            } else {
                match timeout.poll_unpin(cx) {
                    Poll::Ready(_) => Poll::Ready(Err(())),
                    Poll::Pending => Poll::Pending,
                }
            }
        });

        if let Ok(commits) = fu.await {
            let commit = self.valid_precommits(commits.clone());

            if let Some(hash) = commit.target_hash {
                global.lock().decision.insert(height, hash.clone());
                global.lock().height = global.lock().height + num::one();
                global.lock().locked_value = None;
                global.lock().locked_round = None;
                global.lock().valid_value = None;
                global.lock().valid_round = None;

                let f_commit = FinalizedCommit {
                    commits,
                    target_hash: hash,
                    target_number: commit.target_height,
                };
                Ok(f_commit)
            } else {
                Err(self.round_state.lock().prevotes.clone())
            }
        } else {
            // TODO: Return round message log
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
        let SignedMessage { id, msg, signature } = signed_msg;
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
    }
}

#[cfg(test)]
mod test {
    use futures::{executor::LocalPool, task::SpawnExt, StreamExt};
    use tracing::info;

    use crate::testing::GENESIS_HASH;
    use std::sync::Arc;

    use crate::testing::{environment::DummyEnvironment, network::make_network};

    use super::*;

    #[tokio::test]
    async fn basic_test() {
        let subscriber = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::TRACE)
            .finish();

        tracing::subscriber::set_global_default(subscriber)
            .map_err(|_err| eprintln!("Unable to set global default subscriber"));

        info!("Starting test");
        tokio::time::sleep(Duration::from_secs(1)).await;
        info!("Starting test");

        let local_id = 5;
        let voter_set = Arc::new(Mutex::new(VoterSet::new(vec![5]).unwrap()));

        let (network, routing_network) = make_network();

        let env = Arc::new(DummyEnvironment::new(network, local_id, voter_set));

        // init chain
        let last_finalized = env.with_chain(|chain| {
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
                futures::future::ready(n < 6)
            })
            .for_each(|v| {
                log::info!("v: {:?}", v);
                futures::future::ready(())
            })
            .await
    }
}
