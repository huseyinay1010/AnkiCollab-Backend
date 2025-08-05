# Nutze die neueste stabile Rust-Version
FROM rust:1.85-slim

# Setze das Arbeitsverzeichnis im Container
WORKDIR /app

# Kopiere alle Projektdateien in den Container
COPY . .

# Führe den Build-Befehl aus, um dein Projekt zu kompilieren
# Ersetze "your-project-name" mit dem Namen deines Projekts
RUN cargo build --release

# Setze den Befehl, der beim Start des Containers ausgeführt wird
CMD ["./target/release/your-project-name"]
