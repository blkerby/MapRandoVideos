CREATE TABLE account (
    id serial primary key,
    username varchar(100) NOT NULL,
    token_hash bytea NOT NULL,
    permission varchar(100) NOT NULL,
    created_ts timestamptz default current_timestamp
    active boolean default FALSE,
);

CREATE TABLE video (
    id serial primary key,
    num_parts integer NOT NULL,
    next_part_num integer NOT NULL,
    status varchar(100) NOT NULL,
    created_ts timestamptz NOT NULL default current_timestamp,
    created_account_id integer NOT NULL,
    submitted_ts timestamptz,
    updated_ts timestamptz NOT NULL default current_timestamp,
    updated_account_id integer NOT NULL,
    room_id integer,
    from_node_id integer,
    to_node_id integer,
    strat_id integer,
    note varchar(10000) NOT NULL default '',
    crop_size integer,
    crop_center_x integer,
    crop_center_y integer,
    thumbnail_t integer,
    highlight_start_t integer,
    highlight_end_t integer,
    thumbnail_processed_ts timestamptz,
    highlight_processed_ts timestamptz,
    full_video_processed_ts timestamptz,
    permanent boolean NOT NULL default false,
    priority integer
);

--- Extract of essential metadata from sm-json-data, kept up-to-date by `sm-json-data-updater`:

CREATE TABLE area (
    area_id integer PRIMARY KEY,
    name varchar(1000)
);

CREATE TABLE room (
    room_id integer PRIMARY KEY,
    area_id integer,
    name varchar(1000)
);

CREATE TABLE node (
    room_id integer,
    node_id integer,
    name varchar(1000),
    PRIMARY KEY (room_id, node_id)
);

CREATE TABLE strat (
    room_id integer,
    strat_id integer,
    from_node_id integer,
    to_node_id integer,
    name varchar(1000),
    PRIMARY KEY (room_id, strat_id)
);

CREATE TABLE tech (
    tech_id integer PRIMARY KEY,
    name varchar(1000)
);

CREATE TABLE notable (
    room_id integer,
    notable_id integer,
    name varchar(1000),
    PRIMARY KEY (room_id, notable_id)
);

CREATE TABLE notable_strat (
    room_id integer,
    notable_id integer,
    strat_id integer,
    PRIMARY KEY (room_id, notable_id, strat_id)
);

--- Tech/notable difficulty and video settings

CREATE TABLE tech_setting (
    tech_id integer PRIMARY KEY,
    difficulty varchar(1000),
    video_id integer
);

CREATE TABLE notable_setting (
    room_id integer,
    notable_id integer,
    difficulty varchar(1000),
    video_id integer,
    PRIMARY KEY (room_id, notable_id)
);
