// Copyright (C) 2019-2022 Aleo Systems Inc.
// This file is part of the snarkOS library.

// The snarkOS library is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// The snarkOS library is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE. See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with the snarkOS library. If not, see <https://www.gnu.org/licenses/>.

use super::*;

use snarkos_node_messages::{
    BlockRequest,
    BlockResponse,
    Data,
    DataBlocks,
    DisconnectReason,
    Message,
    MessageCodec,
    Ping,
    Pong,
};
use snarkos_node_router::ALEO_MAXIMUM_FORK_DEPTH;
use snarkos_node_tcp::{Connection, ConnectionSide, Tcp};
use snarkvm::prelude::{error, Network};

use futures_util::sink::SinkExt;
use std::{io, net::SocketAddr, time::Duration};

impl<N: Network, C: ConsensusStorage<N>> P2P for Validator<N, C> {
    /// Returns a reference to the TCP instance.
    fn tcp(&self) -> &Tcp {
        self.router.tcp()
    }
}

#[async_trait]
impl<N: Network, C: ConsensusStorage<N>> Handshake for Validator<N, C> {
    /// Performs the handshake protocol.
    async fn perform_handshake(&self, mut connection: Connection) -> io::Result<Connection> {
        // Perform the handshake.
        let peer_addr = connection.addr();
        let conn_side = connection.side();
        let stream = self.borrow_stream(&mut connection);
        let genesis_header = self.ledger.get_header(0).map_err(|e| error(format!("{e}")))?;
        let (peer_ip, mut framed) = self.router.handshake(peer_addr, stream, conn_side, genesis_header).await?;

        // Retrieve the block locators.
        let block_locators = match crate::helpers::get_block_locators(&self.ledger) {
            Ok(block_locators) => Some(block_locators),
            Err(e) => {
                error!("Failed to get block locators: {e}");
                return Err(error(format!("Failed to get block locators: {e}")));
            }
        };

        // Send the first `Ping` message to the peer.
        let message = Message::Ping(Ping::<N> {
            version: Message::<N>::VERSION,
            fork_depth: ALEO_MAXIMUM_FORK_DEPTH,
            node_type: self.node_type(),
            status: self.status(),
            block_locators,
        });
        trace!("Sending '{}' to '{peer_ip}'", message.name());
        framed.send(message).await?;

        Ok(connection)
    }
}

#[async_trait]
impl<N: Network, C: ConsensusStorage<N>> Disconnect for Validator<N, C> {
    /// Any extra operations to be performed during a disconnect.
    async fn handle_disconnect(&self, peer_addr: SocketAddr) {
        self.router.remove_connected_peer(peer_addr);
    }
}

#[async_trait]
impl<N: Network, C: ConsensusStorage<N>> Writing for Validator<N, C> {
    type Codec = MessageCodec<N>;
    type Message = Message<N>;

    /// Creates an [`Encoder`] used to write the outbound messages to the target stream.
    /// The `side` parameter indicates the connection side **from the node's perspective**.
    fn codec(&self, _addr: SocketAddr, _side: ConnectionSide) -> Self::Codec {
        Default::default()
    }
}

#[async_trait]
impl<N: Network, C: ConsensusStorage<N>> Reading for Validator<N, C> {
    type Codec = MessageCodec<N>;
    type Message = Message<N>;

    /// Creates a [`Decoder`] used to interpret messages from the network.
    /// The `side` param indicates the connection side **from the node's perspective**.
    fn codec(&self, _peer_addr: SocketAddr, _side: ConnectionSide) -> Self::Codec {
        Default::default()
    }

    /// Processes a message received from the network.
    async fn process_message(&self, peer_ip: SocketAddr, message: Self::Message) -> io::Result<()> {
        // Process the message. Disconnect if the peer violated the protocol.
        if let Err(error) = self.inbound(peer_ip, message).await {
            warn!("Disconnecting from '{peer_ip}' - {error}");
            self.send(peer_ip, Message::Disconnect(DisconnectReason::ProtocolViolation.into()));
            // Disconnect from this peer.
            self.router().disconnect(peer_ip);
        }
        Ok(())
    }
}

#[async_trait]
impl<N: Network, C: ConsensusStorage<N>> Routing<N> for Validator<N, C> {}

impl<N: Network, C: ConsensusStorage<N>> Heartbeat<N> for Validator<N, C> {
    /// The maximum number of peers permitted to maintain connections with.
    const MAXIMUM_NUMBER_OF_PEERS: usize = 1_000;
}

impl<N: Network, C: ConsensusStorage<N>> Outbound<N> for Validator<N, C> {
    /// Returns a reference to the router.
    fn router(&self) -> &Router<N> {
        &self.router
    }
}

#[async_trait]
impl<N: Network, C: ConsensusStorage<N>> Inbound<N> for Validator<N, C> {
    /// Retrieves the blocks within the block request range, and returns the block response to the peer.
    fn block_request(&self, peer_ip: SocketAddr, message: BlockRequest) -> bool {
        let BlockRequest { start_height, end_height } = &message;

        // Retrieve the blocks within the requested range.
        let blocks = match self.ledger.get_blocks(*start_height..*end_height) {
            Ok(blocks) => Data::Object(DataBlocks(blocks)),
            Err(error) => {
                error!("Failed to retrieve blocks {start_height} to {end_height} from the ledger - {error}");
                return false;
            }
        };
        // Send the `BlockResponse` message to the peer.
        self.send(peer_ip, Message::BlockResponse(BlockResponse { request: message, blocks }));
        true
    }

    /// Handles a `BlockResponse` message.
    fn block_response(&self, peer_ip: SocketAddr, blocks: Vec<Block<N>>) -> bool {
        // Insert the candidate blocks into the sync pool.
        for block in blocks {
            if let Err(error) = self.router().sync().insert_block_response(peer_ip, block) {
                warn!("{error}");
                return false;
            }
        }

        // Retrieve the latest block height.
        let mut latest_height = self.ledger.latest_height();
        // Try to advance the ledger with the sync pool.
        while let Some(block) = self.router().sync().remove_block_response(latest_height + 1) {
            // Check the next block.
            if let Err(error) = self.consensus.check_next_block(&block) {
                warn!("The next block ({}) is invalid - {error}", block.height());
                break;
            }
            // Attempt to advance to the next block.
            if let Err(error) = self.consensus.advance_to_next_block(&block) {
                warn!("{error}");
                break;
            }
            // Insert the height and hash as canon in the sync pool.
            self.router().sync().insert_canon_locator(block.height(), block.hash());
            // Increment the latest height.
            latest_height += 1;
        }
        true
    }

    /// Sleeps for a period and then sends a `Ping` message to the peer.
    fn pong(&self, peer_ip: SocketAddr, _message: Pong) -> bool {
        // Spawn an asynchronous task for the `Ping` request.
        let self_clone = self.clone();
        tokio::spawn(async move {
            // Sleep for the preset time before sending a `Ping` request.
            tokio::time::sleep(Duration::from_secs(Self::PING_SLEEP_IN_SECS)).await;
            // Retrieve the block locators.
            match crate::helpers::get_block_locators(&self_clone.ledger) {
                // Send a `Ping` message to the peer.
                Ok(block_locators) => self_clone.send_ping(peer_ip, Some(block_locators)),
                Err(e) => error!("Failed to get block locators: {e}"),
            }
        });
        true
    }

    /// Retrieves the latest epoch challenge and latest block, and returns the puzzle response to the peer.
    fn puzzle_request(&self, peer_ip: SocketAddr) -> bool {
        // Send the latest puzzle response, if it exists.
        if let Some(puzzle_response) = self.latest_puzzle_response.read().clone() {
            // Send the `PuzzleResponse` message to the peer.
            self.send(peer_ip, Message::PuzzleResponse(puzzle_response));
        }
        true
    }

    /// Saves the latest epoch challenge and latest block in the node.
    fn puzzle_response(&self, peer_ip: SocketAddr, serialized: PuzzleResponse<N>, block: Block<N>) -> bool {
        // Retrieve the epoch number.
        let epoch_number = serialized.epoch_challenge.epoch_number();
        // Retrieve the block height.
        let block_height = block.height();

        info!(
            "Coinbase Puzzle (Epoch {epoch_number}, Block {block_height}, Coinbase Target {}, Proof Target {})",
            block.coinbase_target(),
            block.proof_target()
        );

        // Save the latest epoch challenge in the node.
        self.latest_epoch_challenge.write().replace(serialized.epoch_challenge.clone());
        // Save the latest block in the node.
        self.latest_block.write().replace(block);
        // Save the latest puzzle response in the node.
        self.latest_puzzle_response.write().replace(serialized);

        trace!("Received 'PuzzleResponse' from '{peer_ip}' (Epoch {epoch_number}, Block {block_height})");
        true
    }

    /// Propagates the unconfirmed solution to all connected beacons.
    async fn unconfirmed_solution(
        &self,
        peer_ip: SocketAddr,
        serialized: UnconfirmedSolution<N>,
        solution: ProverSolution<N>,
    ) -> bool {
        // Retrieve the latest epoch challenge.
        let epoch_challenge = self.latest_epoch_challenge.read().clone();
        // Retrieve the latest proof target.
        let proof_target = self.latest_block.read().as_ref().map(|block| block.proof_target());

        if let (Some(epoch_challenge), Some(proof_target)) = (epoch_challenge, proof_target) {
            // Ensure that the prover solution is valid for the given epoch.
            let coinbase_puzzle = self.coinbase_puzzle.clone();
            let is_valid = tokio::task::spawn_blocking(move || {
                solution.verify(coinbase_puzzle.coinbase_verifying_key(), &epoch_challenge, proof_target)
            })
            .await;

            match is_valid {
                // If the solution is valid, propagate the `UnconfirmedSolution` to connected beacons.
                Ok(Ok(true)) => self.propagate_to_beacons(Message::UnconfirmedSolution(serialized), vec![peer_ip]),
                Ok(Ok(false)) | Ok(Err(_)) => {
                    trace!("Invalid prover solution '{}' for the proof target.", solution.commitment())
                }
                Err(error) => warn!("Failed to verify the prover solution: {error}"),
            }
        }
        true
    }
}
