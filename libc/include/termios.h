#ifndef _OS_TERMIOS_H
#define _OS_TERMIOS_H
typedef unsigned char cc_t;
typedef unsigned int speed_t;
typedef unsigned int tcflag_t;
#define NCCS 32
struct termios { tcflag_t c_iflag, c_oflag, c_cflag, c_lflag; cc_t c_line; cc_t c_cc[NCCS]; speed_t c_ispeed, c_ospeed; };
#define VINTR 0
#define VQUIT 1
#define VERASE 2
#define VKILL 3
#define VEOF 4
#define VTIME 5
#define VMIN 6
#define ISIG 0000001
#define ICANON 0000002
#define ECHO 0000010
#define ECHOE 0000020
#define ECHOK 0000040
#define ECHONL 0000100
#define NOFLSH 0000200
#define IXON 0002000
#define ICRNL 0000400
#define INLCR 0000100
#define IGNCR 0000200
#define OPOST 0000001
#define ONLCR 0000004
#define CS8 0000060
#define CSIZE 0000060
#define B0 0
#define B50 1
#define B75 2
#define B110 3
#define B134 4
#define B150 5
#define B200 6
#define B300 7
#define B600 8
#define B1200 9
#define B1800 10
#define B2400 11
#define B4800 12
#define BRKINT 0000002
#define IXOFF 0010000
#define IXANY 0004000
#define IGNBRK 0000001
#define IGNPAR 0000004
#define PARMRK 0000010
#define INPCK 0000020
#define ISTRIP 0000040
#define IUCLC 0001000
#define CREAD 0000200
#define CLOCAL 0004000
#define HUPCL 0002000
#define PARENB 0000400
#define PARODD 0001000
#define CSTOPB 0000100
#define CBAUD 0010017
#define ECHOCTL 0001000
#define ECHOKE 0004000
#define ECHOPRT 0002000
#define IEXTEN 0100000
#define TOSTOP 0000400
#define B9600 0000015
#define B38400 0000017
#define TCSANOW 0
#define TCSADRAIN 1
#define TCSAFLUSH 2
#define TCIFLUSH 0
#define TCOFLUSH 1
#define TCIOFLUSH 2
int tcgetattr(int fd, struct termios *t);
int tcsetattr(int fd, int actions, const struct termios *t);
int tcflush(int fd, int queue);
speed_t cfgetispeed(const struct termios *t);
speed_t cfgetospeed(const struct termios *t);
int cfsetispeed(struct termios *t, speed_t s);
int cfsetospeed(struct termios *t, speed_t s);
#endif
