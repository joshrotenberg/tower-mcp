-- Seed data for sql-mcp Postgres testing

CREATE TABLE users (
    id SERIAL PRIMARY KEY,
    name TEXT NOT NULL,
    email TEXT UNIQUE NOT NULL,
    role TEXT NOT NULL DEFAULT 'member',
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE TABLE products (
    id SERIAL PRIMARY KEY,
    name TEXT NOT NULL,
    description TEXT,
    price NUMERIC(10, 2) NOT NULL,
    category TEXT NOT NULL,
    in_stock BOOLEAN NOT NULL DEFAULT true,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE TABLE orders (
    id SERIAL PRIMARY KEY,
    user_id INTEGER NOT NULL REFERENCES users(id),
    product_id INTEGER NOT NULL REFERENCES products(id),
    quantity INTEGER NOT NULL DEFAULT 1,
    total NUMERIC(10, 2) NOT NULL,
    status TEXT NOT NULL DEFAULT 'pending',
    ordered_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE INDEX idx_orders_user ON orders(user_id);
CREATE INDEX idx_orders_product ON orders(product_id);
CREATE INDEX idx_orders_status ON orders(status);
CREATE INDEX idx_products_category ON products(category);

INSERT INTO users (name, email, role) VALUES
    ('Alice Johnson', 'alice@example.com', 'admin'),
    ('Bob Smith', 'bob@example.com', 'member'),
    ('Charlie Brown', 'charlie@example.com', 'member'),
    ('Diana Prince', 'diana@example.com', 'moderator'),
    ('Eve Wilson', 'eve@example.com', 'member');

INSERT INTO products (name, description, price, category) VALUES
    ('Widget A', 'Standard widget', 9.99, 'widgets'),
    ('Widget B', 'Premium widget with extras', 19.99, 'widgets'),
    ('Gadget X', 'Entry-level gadget', 29.99, 'gadgets'),
    ('Gadget Y', 'Professional gadget', 49.99, 'gadgets'),
    ('Doohickey', 'Multi-purpose doohickey', 14.99, 'accessories'),
    ('Thingamajig', 'Essential thingamajig', 7.99, 'accessories');

INSERT INTO orders (user_id, product_id, quantity, total, status) VALUES
    (1, 1, 2, 19.98, 'completed'),
    (1, 3, 1, 29.99, 'completed'),
    (2, 2, 1, 19.99, 'shipped'),
    (3, 4, 1, 49.99, 'pending'),
    (3, 5, 3, 44.97, 'pending'),
    (4, 1, 5, 49.95, 'completed'),
    (5, 6, 2, 15.98, 'cancelled');
