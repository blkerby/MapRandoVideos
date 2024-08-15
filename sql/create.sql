CREATE TABLE account (
    id serial primary key,
    username varchar(100) NOT NULL,
    token varchar(100) NOT NULL,
    discord_username varchar(100),
    permission varchar(100) NOT NULL,
    created timestamp default current_timestamp,
    login_ts timestamp
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
    permanent boolean NOT NULL default false
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
