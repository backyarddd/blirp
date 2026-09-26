-- Subset of opencode's schema (tables and columns blirp reads) with a
-- synthetic parent session, one child (subagent) session and a message
-- still streaming. {{NOW_MS}} keeps the session "recently updated".
CREATE TABLE `session` (
  `id` text PRIMARY KEY,
  `project_id` text NOT NULL,
  `workspace_id` text,
  `parent_id` text,
  `slug` text NOT NULL,
  `directory` text NOT NULL,
  `path` text,
  `title` text NOT NULL,
  `version` text NOT NULL,
  `cost` real DEFAULT 0 NOT NULL,
  `tokens_input` integer DEFAULT 0 NOT NULL,
  `tokens_output` integer DEFAULT 0 NOT NULL,
  `tokens_reasoning` integer DEFAULT 0 NOT NULL,
  `tokens_cache_read` integer DEFAULT 0 NOT NULL,
  `tokens_cache_write` integer DEFAULT 0 NOT NULL,
  `agent` text,
  `model` text,
  `time_created` integer NOT NULL,
  `time_updated` integer NOT NULL,
  `time_archived` integer
);
CREATE TABLE `message` (
  `id` text PRIMARY KEY,
  `session_id` text NOT NULL,
  `time_created` integer NOT NULL,
  `time_updated` integer NOT NULL,
  `data` text NOT NULL
);
CREATE TABLE `part` (
  `id` text PRIMARY KEY,
  `message_id` text NOT NULL,
  `session_id` text NOT NULL,
  `time_created` integer NOT NULL,
  `time_updated` integer NOT NULL,
  `data` text NOT NULL
);
CREATE INDEX `message_session_time_created_id_idx` ON `message` (`session_id`,`time_created`,`id`);
CREATE INDEX `part_message_id_id_idx` ON `part` (`message_id`,`id`);

INSERT INTO session VALUES ('ses_parent','prj_1',NULL,NULL,'calm-otter','{{CWD_RAW}}',NULL,'New session - 2026-01-03T08:00:00.000Z','1.2.0',0.05,400,60,5,1000,0,'build','claude-sonnet-4-5',1767427200000,{{NOW_MS}},NULL);
INSERT INTO session VALUES ('ses_child','prj_1',NULL,'ses_parent','quiet-lynx','{{CWD_RAW}}',NULL,'Explore config (@explore subagent)','1.2.0',0.001,20,5,0,0,0,'explore','claude-haiku-4-5',1767427210000,1767427220000,NULL);

INSERT INTO message VALUES ('msg_001','ses_parent',1767427200000,1767427200000,'{"role":"user","time":{"created":1767427200000},"agent":"build","model":{"providerID":"anthropic","modelID":"claude-sonnet-4-5"}}');
INSERT INTO part VALUES ('prt_001','msg_001','ses_parent',1767427200000,1767427200000,'{"type":"text","text":"Refactor the parser\ntoken {{SECRET}}"}');
INSERT INTO part VALUES ('prt_001b','msg_001','ses_parent',1767427200000,1767427200000,'{"type":"text","text":"(file contents)","synthetic":true}');

INSERT INTO message VALUES ('msg_002','ses_parent',1767427201000,1767427209000,'{"role":"assistant","mode":"build","agent":"build","path":{"cwd":"{{CWD}}","root":"{{CWD}}"},"cost":0.05,"tokens":{"total":1465,"input":400,"output":60,"reasoning":5,"cache":{"write":0,"read":1000}},"modelID":"claude-sonnet-4-5","providerID":"anthropic","time":{"created":1767427201000,"completed":1767427209000},"finish":"stop"}');
INSERT INTO part VALUES ('prt_002','msg_002','ses_parent',1767427201000,1767427201000,'{"type":"step-start"}');
INSERT INTO part VALUES ('prt_003','msg_002','ses_parent',1767427201000,1767427201000,'{"type":"reasoning","text":"thinking","time":{"start":1767427201000,"end":1767427202000}}');
INSERT INTO part VALUES ('prt_004','msg_002','ses_parent',1767427202000,1767427202000,'{"type":"text","text":"Updating parser.rs"}');
INSERT INTO part VALUES ('prt_005','msg_002','ses_parent',1767427203000,1767427204000,'{"type":"tool","tool":"edit","callID":"call_a","state":{"status":"completed","input":{"filePath":"src/parser.rs","oldString":"a","newString":"b"},"output":"Edit applied successfully.","title":"src/parser.rs","metadata":{},"time":{"start":1767427203000,"end":1767427204000}}}');
INSERT INTO part VALUES ('prt_006','msg_002','ses_parent',1767427205000,1767427206000,'{"type":"tool","tool":"bash","callID":"call_b","state":{"status":"error","input":{"command":"cargo test"},"error":"exit code 101","time":{"start":1767427205000,"end":1767427206000}}}');
INSERT INTO part VALUES ('prt_007','msg_002','ses_parent',1767427207000,1767427207000,'{"type":"patch","hash":"abc","files":["src/parser.rs"]}');
INSERT INTO part VALUES ('prt_008','msg_002','ses_parent',1767427208000,1767427208000,'{"type":"step-finish","reason":"stop","tokens":{"input":400,"output":60},"cost":0.05}');

INSERT INTO message VALUES ('msg_003','ses_parent',1767427210000,1767427210000,'{"role":"assistant","mode":"build","agent":"build","modelID":"claude-sonnet-4-5","providerID":"anthropic","time":{"created":1767427210000}}');
INSERT INTO part VALUES ('prt_009','msg_003','ses_parent',1767427210000,1767427210000,'{"type":"text","text":"Running the tests now"}');

INSERT INTO message VALUES ('msg_c01','ses_child',1767427210000,1767427210000,'{"role":"user","time":{"created":1767427210000}}');
INSERT INTO part VALUES ('prt_c01','msg_c01','ses_child',1767427210000,1767427210000,'{"type":"text","text":"Explore the config"}');
INSERT INTO message VALUES ('msg_c02','ses_child',1767427211000,1767427215000,'{"role":"assistant","time":{"created":1767427211000,"completed":1767427215000},"finish":"stop"}');
INSERT INTO part VALUES ('prt_c02','msg_c02','ses_child',1767427212000,1767427212000,'{"type":"text","text":"config.toml holds the settings"}');
