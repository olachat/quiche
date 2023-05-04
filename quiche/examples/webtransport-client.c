// Copyright (C) 2019, Cloudflare, Inc.
// All rights reserved.
//
// Redistribution and use in source and binary forms, with or without
// modification, are permitted provided that the following conditions are
// met:
//
//     * Redistributions of source code must retain the above copyright
//       notice, this list of conditions and the following disclaimer.
//
//     * Redistributions in binary form must reproduce the above copyright
//       notice, this list of conditions and the following disclaimer in the
//       documentation and/or other materials provided with the distribution.
//
// THIS SOFTWARE IS PROVIDED BY THE COPYRIGHT HOLDERS AND CONTRIBUTORS "AS
// IS" AND ANY EXPRESS OR IMPLIED WARRANTIES, INCLUDING, BUT NOT LIMITED TO,
// THE IMPLIED WARRANTIES OF MERCHANTABILITY AND FITNESS FOR A PARTICULAR
// PURPOSE ARE DISCLAIMED. IN NO EVENT SHALL THE COPYRIGHT HOLDER OR
// CONTRIBUTORS BE LIABLE FOR ANY DIRECT, INDIRECT, INCIDENTAL, SPECIAL,
// EXEMPLARY, OR CONSEQUENTIAL DAMAGES (INCLUDING, BUT NOT LIMITED TO,
// PROCUREMENT OF SUBSTITUTE GOODS OR SERVICES; LOSS OF USE, DATA, OR
// PROFITS; OR BUSINESS INTERRUPTION) HOWEVER CAUSED AND ON ANY THEORY OF
// LIABILITY, WHETHER IN CONTRACT, STRICT LIABILITY, OR TORT (INCLUDING
// NEGLIGENCE OR OTHERWISE) ARISING IN ANY WAY OUT OF THE USE OF THIS
// SOFTWARE, EVEN IF ADVISED OF THE POSSIBILITY OF SUCH DAMAGE.

#include <inttypes.h>
#include <stdio.h>
#include <stdlib.h>
#include <stdint.h>
#include <stdbool.h>
#include <unistd.h>

#include <fcntl.h>
#include <errno.h>

#include <sys/types.h>
#include <sys/socket.h>
#include <netdb.h>

#include <ev.h>

#include <quiche.h>

#define LOCAL_CONN_ID_LEN 16

#define MAX_DATAGRAM_SIZE 1350

struct conn_io {
    ev_timer timer;

    const char *host;

    int sock;

    struct sockaddr_storage local_addr;
    socklen_t local_addr_len;

    quiche_conn *conn;

    client_session *client_session;
};

static void debug_log(const char *line, void *argp) {
    fprintf(stderr, "%s\n", line);
}

static void flush_egress(struct ev_loop *loop, struct conn_io *conn_io) {
    static uint8_t out[MAX_DATAGRAM_SIZE];

    quiche_send_info send_info;

    while (1) {
        ssize_t written = quiche_conn_send(conn_io->conn, out, sizeof(out),
                                           &send_info);

        if (written == QUICHE_ERR_DONE) {
            fprintf(stderr, "done writing\n");
            break;
        }

        if (written < 0) {
            fprintf(stderr, "failed to create packet: %zd\n", written);
            return;
        }

        ssize_t sent = sendto(conn_io->sock, out, written, 0,
                              (struct sockaddr *) &send_info.to,
                              send_info.to_len);

        if (sent != written) {
            perror("failed to send");
            return;
        }

        fprintf(stderr, "sent %zd bytes\n", sent);
    }

    double t = quiche_conn_timeout_as_nanos(conn_io->conn) / 1e9f;
    conn_io->timer.repeat = t;
    ev_timer_again(loop, &conn_io->timer);
}



static void recv_cb(EV_P_ ev_io *w, int revents) {
    static bool connect_request_sent = false;

    struct conn_io *conn_io = w->data;

    static uint8_t buf[65535];

    while (1) {
        struct sockaddr_storage peer_addr;
        socklen_t peer_addr_len = sizeof(peer_addr);
        memset(&peer_addr, 0, peer_addr_len);

        ssize_t read = recvfrom(conn_io->sock, buf, sizeof(buf), 0,
                                (struct sockaddr *) &peer_addr,
                                &peer_addr_len);

        if (read < 0) {
            if ((errno == EWOULDBLOCK) || (errno == EAGAIN)) {
                fprintf(stderr, "recv would block\n");
                break;
            }

            perror("failed to read");
            return;
        }

        quiche_recv_info recv_info = {
            (struct sockaddr *) &peer_addr,
            peer_addr_len,

            (struct sockaddr *) &conn_io->local_addr,
            conn_io->local_addr_len,
        };

        ssize_t done = quiche_conn_recv(conn_io->conn, buf, read, &recv_info);

        if (done < 0) {
            fprintf(stderr, "failed to process packet: %zd\n", done);
            continue;
        }

        fprintf(stderr, "recv %zd bytes\n", done);
    }

    fprintf(stderr, "done reading\n");

    if (quiche_conn_is_closed(conn_io->conn)) {
        fprintf(stderr, "connection closed\n");

        ev_break(EV_A_ EVBREAK_ONE);
        return;
    }

    if (quiche_conn_is_established(conn_io->conn) && conn_io->client_session == NULL) {
        const uint8_t *app_proto;
        size_t app_proto_len;

        quiche_conn_application_proto(conn_io->conn, &app_proto, &app_proto_len);

        fprintf(stderr, "connection established: %.*s\n",
                (int) app_proto_len, app_proto);


        conn_io->client_session = quiche_h3_webtransport_clientsession_with_transport(conn_io->conn);
        if (conn_io->client_session == NULL) {
            fprintf(stderr, "failed to create WebTransport Session\n");
            return;
        }

        if(!connect_request_sent) {
            connect_request_sent = true;
            connection_request request = {
                .authority = "example.com",
                .path = "/",
                .origin = "localhost"
            };
            quiche_h3_webtransport_clientsession_send_connect_request(conn_io->client_session, conn_io->conn, &request);
        }
    }

    if (quiche_conn_is_established(conn_io->conn) && conn_io->client_session != NULL) {
        webtransport_clientevent_event *ev;

        while (1) {
            int64_t ret = quiche_h3_webtransport_clientsession_poll(conn_io->client_session,
                                            conn_io->conn,
                                            &ev);
            if(ret != 0) {
                break;
            }

            uint64_t sid = 0;
            int rc = quiche_h3_webtransport_clientevent_event(ev, &sid);
            fprintf(stderr, "quiche_h3_webtransport_clientevent_event  %d\n", rc);

            switch (rc) {
                case QUICHE_H3_WEBTRANSPORT_CLIENTEVENT_PEER_READY: {
                    // connection_request request = {
                    //     .authority = "example.com",
                    //     .path = "/",
                    //     .origin = "localhost"
                    // };
                    // quiche_h3_webtransport_clientsession_send_connect_request(conn_io->client_session, conn_io->conn, &request);
                    
                    fprintf(stderr, "peer ready\n");
                    break;
                }
                case QUICHE_H3_WEBTRANSPORT_CLIENTEVENT_CONNECTED: {
                    
                    fprintf(stderr, "webtransport connected\n");

                    int stream_id = quiche_h3_webtransport_clientsession_open_stream(conn_io->client_session,conn_io->conn, true);
                    if (stream_id < 0) {
                        fprintf(stderr, "failed to close connection\n");
                    }
                    char helloworld[] = "helloworld";
                    int rc = quiche_h3_webtransport_clientsession_send_stream_data(conn_io->client_session,conn_io->conn,stream_id, (uint8_t *)helloworld, strlen(helloworld));
                    if (rc < 0) {
                        fprintf(stderr, "failed to send stream data\n");
                    }
                    break;
                }
               
                case QUICHE_H3_WEBTRANSPORT_CLIENTEVENT_STREAMFINISHED: {
                    fprintf(stderr, "webtransport stream finished\n");
                    break;
                }
                case QUICHE_H3_WEBTRANSPORT_CLIENTEVENT_DATAGRAM: {
                    for (;;) {
                        bool in_session = false;
                        ssize_t offset = 0;
                        ssize_t len = quiche_h3_webtransport_clientsession_recv_dgram(conn_io->client_session,
                                                          conn_io->conn, 
                                                          buf, sizeof(buf),
                                                          &in_session, &offset);

                        if (len <= 0) {
                            break;
                        }

                        if(in_session) {
                            printf("webtransport datagram: %.*s", (int) len, buf);
                        }
                        
                    }
                    break;
                }

                case QUICHE_H3_WEBTRANSPORT_CLIENTEVENT_STREAMDATA: {
                    for (;;) {
                        ssize_t len = quiche_h3_webtransport_clientsession_recv_stream_data(conn_io->client_session,
                                                          conn_io->conn, 
                                                          sid,
                                                          buf, sizeof(buf));

                        if (len <= 0) {
                            break;
                        }

                        printf("webtransport stream data: %.*s", (int) len, buf);
                        
                        
                    }
                    break;
                }

                case QUICHE_H3_WEBTRANSPORT_CLIENTEVENT_SESSIONFINISHED:
                    // if (quiche_conn_close(conn_io->conn, true, 0, NULL, 0) < 0) {
                    //     fprintf(stderr, "failed to close connection\n");
                    // }
                    break;

                case QUICHE_H3_WEBTRANSPORT_CLIENTEVENT_SESSIONRESET:
                    fprintf(stdout, "request was reset\n");

                    // if (quiche_conn_close(conn_io->conn, true, 0, NULL, 0) < 0) {
                    //     fprintf(stderr, "failed to close connection\n");
                    // }
                    break;

                case QUICHE_H3_WEBTRANSPORT_CLIENTEVENT_SESSIONGOAWAY: {
                    fprintf(stdout, "got GOAWAY\n");

                    // if (quiche_conn_close(conn_io->conn, true, 0, NULL, 0) < 0) {
                    //     fprintf(stderr, "failed to close connection\n");
                    // }
                    break;
                }
            }

            quiche_h3_webtransport_clientsevent_free(ev);
        }
    }

    flush_egress(loop, conn_io);
}

static void timeout_cb(EV_P_ ev_timer *w, int revents) {
    struct conn_io *conn_io = w->data;
    quiche_conn_on_timeout(conn_io->conn);

    fprintf(stderr, "timeout\n");

    flush_egress(loop, conn_io);

    if (quiche_conn_is_closed(conn_io->conn)) {
        quiche_stats stats;
        quiche_path_stats path_stats;

        quiche_conn_stats(conn_io->conn, &stats);
        quiche_conn_path_stats(conn_io->conn, 0, &path_stats);

        fprintf(stderr, "connection closed, recv=%zu sent=%zu lost=%zu rtt=%" PRIu64 "ns\n",
                stats.recv, stats.sent, stats.lost, path_stats.rtt);

        ev_break(EV_A_ EVBREAK_ONE);
        return;
    }
}

int main(int argc, char *argv[]) {
    const char *host = argv[1];
    const char *port = argv[2];

    const struct addrinfo hints = {
        .ai_family = PF_UNSPEC,
        .ai_socktype = SOCK_DGRAM,
        .ai_protocol = IPPROTO_UDP
    };

    quiche_enable_debug_logging(debug_log, NULL);

    struct addrinfo *peer;
    if (getaddrinfo(host, port, &hints, &peer) != 0) {
        perror("failed to resolve host");
        return -1;
    }

    int sock = socket(peer->ai_family, SOCK_DGRAM, 0);
    if (sock < 0) {
        perror("failed to create socket");
        return -1;
    }

    if (fcntl(sock, F_SETFL, O_NONBLOCK) != 0) {
        perror("failed to make socket non-blocking");
        return -1;
    }

    quiche_config *config = quiche_config_new(0xbabababa);
    if (config == NULL) {
        fprintf(stderr, "failed to create config\n");
        return -1;
    }

    quiche_config_set_application_protos(config,
        (uint8_t *) QUICHE_H3_APPLICATION_PROTOCOL,
        sizeof(QUICHE_H3_APPLICATION_PROTOCOL) - 1);

    quiche_config_set_max_idle_timeout(config, 5000);
    quiche_config_set_max_recv_udp_payload_size(config, MAX_DATAGRAM_SIZE);
    quiche_config_set_max_send_udp_payload_size(config, MAX_DATAGRAM_SIZE);
    quiche_config_set_initial_max_data(config, 10000000);
    quiche_config_set_initial_max_stream_data_bidi_local(config, 1000000);
    quiche_config_set_initial_max_stream_data_bidi_remote(config, 1000000);
    quiche_config_set_initial_max_stream_data_uni(config, 1000000);
    quiche_config_set_initial_max_streams_bidi(config, 100);
    quiche_config_set_initial_max_streams_uni(config, 100);
    quiche_config_set_disable_active_migration(config, true);
    quiche_config_enable_dgram(config, true, 65536, 65536);

    if (getenv("SSLKEYLOGFILE")) {
      quiche_config_log_keys(config);
    }

    // ABC: old config creation here

    uint8_t scid[LOCAL_CONN_ID_LEN];
    int rng = open("/dev/urandom", O_RDONLY);
    if (rng < 0) {
        perror("failed to open /dev/urandom");
        return -1;
    }

    ssize_t rand_len = read(rng, &scid, sizeof(scid));
    if (rand_len < 0) {
        perror("failed to create connection ID");
        return -1;
    }

    struct conn_io *conn_io = malloc(sizeof(*conn_io));
    if (conn_io == NULL) {
        fprintf(stderr, "failed to allocate connection IO\n");
        return -1;
    }

    conn_io->local_addr_len = sizeof(conn_io->local_addr);
    if (getsockname(sock, (struct sockaddr *)&conn_io->local_addr,
                    &conn_io->local_addr_len) != 0)
    {
        perror("failed to get local address of socket");
        return -1;
    };

    quiche_conn *conn = quiche_connect(host, (const uint8_t *) scid, sizeof(scid),
                                       (struct sockaddr *) &conn_io->local_addr,
                                       conn_io->local_addr_len,
                                       peer->ai_addr, peer->ai_addrlen, config);

    if (conn == NULL) {
        fprintf(stderr, "failed to create connection\n");
        return -1;
    }

    conn_io->sock = sock;
    conn_io->conn = conn;
    conn_io->host = host;
    conn_io->client_session = NULL;

    ev_io watcher;

    struct ev_loop *loop = ev_default_loop(0);

    ev_io_init(&watcher, recv_cb, conn_io->sock, EV_READ);
    ev_io_start(loop, &watcher);
    watcher.data = conn_io;

    ev_init(&conn_io->timer, timeout_cb);
    conn_io->timer.data = conn_io;

    flush_egress(loop, conn_io);

    ev_loop(loop, 0);

    freeaddrinfo(peer);

    quiche_h3_webtransport_clientsession_free(conn_io->client_session);
    quiche_conn_free(conn);
    quiche_config_free(config);

    return 0;
}
