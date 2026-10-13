export type RefreshTokenResult = UserResult;

export type UserResult = {
  success: boolean;
  data: {
    avatar: string;
    username: string;
    nickname: string;
    roles: Array<string>;
    permissions: Array<string>;
    accessToken: string;
    refreshToken: string;
    expires: Date;
  };
};

export const getLogin = (_data?: object) => {
  return Promise.resolve({
    success: true,
    data: {
      avatar: "/logo.png",
      username: "admin",
      nickname: "admin",
      roles: ["admin"],
      permissions: ["*:*:*"],
      accessToken: "local-console",
      refreshToken: "local-console",
      expires: new Date(Date.now() + 7 * 86400000)
    }
  } satisfies UserResult);
};
export const refreshTokenApi = (_data?: any) => getLogin();
