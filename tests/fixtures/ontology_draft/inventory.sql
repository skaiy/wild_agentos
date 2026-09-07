CREATE TABLE sites (
    site_id BIGINT PRIMARY KEY,
    name VARCHAR(120) NOT NULL
);

CREATE TABLE assets (
    asset_id UUID NOT NULL,
    site_id BIGINT NOT NULL REFERENCES sites(site_id),
    installed_at TIMESTAMP,
    capacity_kw DECIMAL(10,2),
    active BOOLEAN NOT NULL,
    PRIMARY KEY (asset_id)
);
