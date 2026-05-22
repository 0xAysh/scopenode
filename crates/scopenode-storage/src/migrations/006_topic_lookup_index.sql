-- Cover the JSON-RPC eth_getLogs hot path: contract + topic0 + block range.
CREATE INDEX IF NOT EXISTS idx_events_contract_topic_block_log
    ON events(contract, topic0, block_number, log_index);
