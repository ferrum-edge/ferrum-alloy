CREATE TABLE orders (
    id BIGSERIAL PRIMARY KEY,
    item TEXT NOT NULL CHECK (length(item) BETWEEN 1 AND 200),
    quantity INTEGER NOT NULL CHECK (quantity > 0),
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
