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
    permanent boolean
);
