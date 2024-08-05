CREATE TABLE account (
    id serial primary key,
    username varchar(100) NOT NULL,
    password_hash varchar(100) NOT NULL,
    login_ts timestamp,
    login_token varchar(100),
    discord_username varchar(100),
    permission varchar(100) NOT NULL
);

CREATE TABLE video (
    id serial primary key,
    video_hash varchar(100) NOT NULL,
    frame_count integer,
    created_ts timestamp,
    created_account_id integer,
    updated_ts timestamp,
    updated_account_id integer,
    room_id integer,
    from_node_id integer,
    to_node_id integer,
    strat_id integer,
    notes varchar(10000),
    highlight_x_min integer,
    highlight_x_max integer,
    highlight_y_min integer,
    highlight_y_max integer,
    highlight_t_min integer,
    highlight_t_max integer,
    thumbnail_t integer,
    status varchar(100) NOT NULL,
    permanent boolean
);