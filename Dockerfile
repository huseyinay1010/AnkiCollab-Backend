# Wir verwenden das neueste Rust-Image, das die benötigte Edition2024-Funktion unterstützt.
FROM rust:1.85-bookworm-slim

# Setze das Arbeitsverzeichnis im Container
WORKDIR /app

# Kopiere alle Projektdaten in den Container
COPY . .

# Führe den Build aus, um die Anwendung zu kompilieren
# Mit der --release Flagge wird die Binärdatei optimiert.
RUN cargo build --release

# Setze den Befehl, der beim Start des Containers ausgeführt wird.
# Ersetze den Platzhalter 'your_binary_name' mit dem Namen der ausführbaren Datei deines Projekts.
# Der Name ist normalerweise der gleiche wie der in deiner Cargo.toml.
CMD ["./target/release/AnkiCollab-Backend"]
