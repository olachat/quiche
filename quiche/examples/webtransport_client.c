#include <quiche.h>
#include <stdio.h>
#include <string.h>

// Callback function for handling received data
void on_data_received(const char* data, size_t len) {
    printf("Received data: %.*s\n", (int)len, data);
}

// Callback function for handling client close
void on_close() {
    printf("Connection closed.\n");
}

int main() {
    // Create a new WebTransport client instance
    WebTransportClient* client = webtransport_client_new();
    if (client == NULL) {
        printf("Failed to create WebTransport client instance.\n");
        return 1;
    }

    // Connect to a WebTransport server
    const char* server_addr = "example.com:443";
    int ret = webtransport_client_connect(client, server_addr);
    if (ret < 0) {
        printf("Failed to connect to server.\n");
        webtransport_client_free(client);
        return 1;
    }

    // Send data to the WebTransport server
    const char* data = "Hello, server!";
    size_t len = strlen(data);
    ssize_t bytes_sent = webtransport_client_send_data(client, data, len);
    if (bytes_sent < 0) {
        printf("Failed to send data to server.\n");
        webtransport_client_free(client);
        return 1;
    }
    printf("Sent %zd bytes of data to server.\n", bytes_sent);

    // Set callback functions for handling received data and client close
    webtransport_client_set_data_received_callback(client, on_data_received);
    webtransport_client_set_close_callback(client, on_close);

    // Run the WebTransport client event loop
    ret = webtransport_client_run_event_loop(client);
    if (ret < 0) {
        printf("Failed to run event loop.\n");
        webtransport_client_free(client);
        return 1;
    }

    // Stop the WebTransport client event loop
    webtransport_client_stop_event_loop(client);

    // Free the WebTransport client instance
    webtransport_client_free(client);

    return 0;
}
