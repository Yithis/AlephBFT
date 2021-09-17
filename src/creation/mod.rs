use crate::{
    config::{Config as GeneralConfig, DelaySchedule},
    nodes::{NodeCount, NodeIndex},
    runway::NotificationOut,
    units::Unit,
    Hasher, Receiver, Round, Sender,
};
use futures::{channel::oneshot, FutureExt, StreamExt};
use futures_timer::Delay;
use log::{debug, error, info, warn};
use std::time::Duration;

mod creator;

use creator::Creator;

/// The configuration needed for the process creating new units.
pub struct Config {
    node_id: NodeIndex,
    n_members: NodeCount,
    create_lag: DelaySchedule,
    max_round: Round,
}

impl From<GeneralConfig> for Config {
    fn from(conf: GeneralConfig) -> Self {
        Config {
            node_id: conf.node_ix,
            n_members: conf.n_members,
            create_lag: conf.delay_config.unit_creation_delay,
            max_round: conf.max_round,
        }
    }
}

pub struct IO<H: Hasher> {
    pub(crate) incoming_parents: Receiver<Unit<H>>,
    pub(crate) outgoing_units: Sender<NotificationOut<H>>,
}

async fn wait_until_ready<H: Hasher>(
    round: Round,
    creator: &mut Creator<H>,
    create_lag: &DelaySchedule,
    incoming_parents: &mut Receiver<Unit<H>>,
    mut exit: &mut oneshot::Receiver<()>,
) -> Result<(), ()> {
    let mut delay = Delay::new(create_lag(round.into())).fuse();
    let mut delay_passed = false;
    while !delay_passed || !creator.can_create(round) {
        futures::select! {
            unit = incoming_parents.next() => match unit {
                Some(unit) => creator.add_unit(&unit),
                None => {
                    info!(target: "AlephBFT-creator", "Incoming parent channel closed, exiting.");
                    return Err(());
                }
            },
            _ = &mut delay => {
                if delay_passed {
                    warn!(target: "AlephBFT-creator", "More than half hour has passed since we created the previous unit.");
                }
                delay_passed = true;
                delay = Delay::new(Duration::from_secs(30 * 60)).fuse();
            },
            _ = exit => {
                info!(target: "AlephBFT-creator", "Received exit signal.");
                return Err(());
            },
        }
    }
    Ok(())
}

/// A process responsible for creating new units. It receives all the units added locally to the Dag
/// via the `incoming_parents` channel. It creates units according to an internal strategy respecting
/// always the following constraints: if round is equal to 0, U has no parents, otherwise for a unit U of round r > 0
/// - all U's parents are from round (r-1),
/// - all U's parents are created by different nodes,
/// - one of U's parents is the (r-1)-round unit by U's creator,
/// - U has > floor(2*N/3) parents.
/// - U will appear in the channel only if all U's parents appeared there before
/// The currently implemented strategy creates the unit U according to a delay schedule and when enough
/// candidates for parents are available for all the above constraints to be satisfied.
///
/// We refer to the documentation https://cardinal-cryptography.github.io/AlephBFT/internals.html
/// Section 5.1 for a discussion of this component.
pub async fn run<H: Hasher>(
    conf: Config,
    io: IO<H>,
    starting_round: oneshot::Receiver<Round>,
    mut exit: oneshot::Receiver<()>,
) {
    let Config {
        node_id,
        n_members,
        create_lag,
        max_round,
    } = conf;
    let mut creator = Creator::new(node_id, n_members);
    let IO {
        mut incoming_parents,
        outgoing_units,
    } = io;
    let starting_round = match starting_round.await {
        Ok(round) => round,
        Err(e) => {
            error!(target: "AlephBFT-creator", "Starting round not provided: {}", e);
            return;
        }
    };
    debug!(target: "AlephBFT-creator", "Creator starting from round {}", starting_round);
    for round in starting_round..max_round {
        if !creator.is_behind(round)
            && wait_until_ready(
                round,
                &mut creator,
                &create_lag,
                &mut incoming_parents,
                &mut exit,
            )
            .await
            .is_err()
        {
            return;
        }
        let (unit, parent_hashes) = creator.create_unit(round);
        if let Err(e) =
            outgoing_units.unbounded_send(NotificationOut::CreatedPreUnit(unit, parent_hashes))
        {
            warn!(target: "AlephBFT-creator", "Notification send error: {}. Exiting.", e);
            return;
        }
    }
    warn!(target: "AlephBFT-creator", "Maximum round reached. Not creating another unit.");
}
