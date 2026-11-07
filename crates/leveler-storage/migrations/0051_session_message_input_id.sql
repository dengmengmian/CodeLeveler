-- Link a transcript row back to the accepted input that produced it.
--
-- The journal row is the stable identity the client saw before the transcript
-- existed. Storing it on the transcript row lets a snapshot rebuild project the
-- same `MessageId` the live event used, so a reconnect or a Resume cannot show
-- a second row for one message.
ALTER TABLE session_messages ADD COLUMN input_id TEXT;

CREATE INDEX idx_session_messages_input
    ON session_messages(session_id, input_id);
